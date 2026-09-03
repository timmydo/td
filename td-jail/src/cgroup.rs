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
const MAX_PROCESS_TOKEN_BYTES: usize = 64;
const MAX_PROCESS_CMDLINE_BYTES: u64 = 64 * 1024;
const MAX_PROCESS_STATUS_BYTES: u64 = 64 * 1024;
const MAX_FIREFOX_CHILDREN: usize = 256;
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
pub(crate) struct CpuStat {
    pub(crate) periods: u64,
    pub(crate) throttled: u64,
    pub(crate) throttled_usec: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Report {
    pub(crate) events: MemoryEvents,
    pub(crate) peak: u64,
    pub(crate) cpu: CpuStat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessSandbox {
    pub(crate) namespace_pid: u32,
    pub(crate) no_new_privileges: u32,
    pub(crate) seccomp: u32,
    pub(crate) filters: u32,
}

impl Report {
    pub(crate) fn diagnostic(self) -> String {
        format!(
            "memory.events high={} max={} oom={} oom_kill={} oom_group_kill={}; memory.peak={}; cpu.stat nr_periods={} nr_throttled={} throttled_usec={}",
            self.events.high,
            self.events.max,
            self.events.oom,
            self.events.oom_kill,
            self.events.oom_group_kill,
            self.peak,
            self.cpu.periods,
            self.cpu.throttled,
            self.cpu.throttled_usec,
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
    let (instance, _, report) = active_instance(application, limits, uid, gid)?;
    Ok(format!(
        "TD-JAIL-RESOURCE-CAPS-OK instance={instance} {}",
        report.diagnostic()
    ))
}

fn active_instance(
    application: &str,
    limits: ResolvedResourceLimits,
    uid: u32,
    gid: u32,
) -> io::Result<(String, PathBuf, Report)> {
    authority::validate_application_name(application)?;
    let root = Path::new(CGROUP_ROOT);
    require_delegation(root, uid, gid)?;
    let entries = fs::read_dir(root)?;
    let mut active: Option<(String, PathBuf, Report)> = None;
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
        let candidate = (name, directory.clone(), report(&directory)?);
        if active.replace(candidate).is_some() {
            return Err(io::Error::other(format!(
                "more than one active cgroup exists for application {application:?}"
            )));
        }
    }
    let Some(active) = active else {
        return Err(io::Error::other(format!(
            "no active cgroup exists for application {application:?}"
        )));
    };
    Ok(active)
}

pub(crate) fn valid_process_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_PROCESS_TOKEN_BYTES
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b':' | b'-')
        })
}

fn command_has_token(command: &[u8], token: &str) -> bool {
    command
        .split(|byte| *byte == 0)
        .any(|argument| argument == token.as_bytes())
}

fn read_process_command(path: &Path) -> Option<Vec<u8>> {
    let mut command = Vec::new();
    if fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_PROCESS_CMDLINE_BYTES.saturating_add(1))
                .read_to_end(&mut command)
        })
        .is_err()
        || command.len() as u64 > MAX_PROCESS_CMDLINE_BYTES
    {
        return None;
    }
    Some(command)
}

fn process_token_evidence(instance: &str, token: &str, pid: u32, starttime: u64) -> String {
    format!(
        "TD-JAIL-PROCESS-TOKEN-OK instance={instance} token={token} pid={pid} \
         starttime={starttime}"
    )
}

fn revalidated_process_starttime(path: &Path, initial: u64) -> Option<u64> {
    let observed = read_process_stat(path)
        .and_then(|stat| process_starttime(&stat))
        .ok()?;
    (observed == initial).then_some(observed)
}

pub(crate) fn probe_process_token(
    application: &str,
    token: &str,
    limits: ResolvedResourceLimits,
    uid: u32,
    gid: u32,
) -> io::Result<String> {
    if !valid_process_token(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process token must be 1..=64 restricted ASCII bytes",
        ));
    }
    let (instance, directory, _) = active_instance(application, limits, uid, gid)?;
    let membership = membership_for_instance(&instance)?;
    let processes = read_control(&directory.join("cgroup.procs"))?;
    for line in processes.lines() {
        let pid = line.parse::<u32>().map_err(|error| {
            io::Error::other(format!(
                "application cgroup {instance:?} has invalid process id {line:?}: {error}"
            ))
        })?;
        let proc_directory = PathBuf::from(format!("/proc/{pid}"));
        let stat_path = proc_directory.join("stat");
        let Ok(starttime) = read_process_stat(&stat_path).and_then(|stat| process_starttime(&stat))
        else {
            continue;
        };
        let Some(command) = read_process_command(&proc_directory.join("cmdline")) else {
            continue;
        };
        if !command_has_token(&command, token) {
            continue;
        }
        let membership_path = format!("/proc/{pid}/cgroup");
        let Ok(observed) = read_bounded(Path::new(&membership_path)) else {
            continue;
        };
        if require_membership_text(&observed, &membership).is_err() {
            continue;
        }
        if revalidated_process_starttime(&stat_path, starttime).is_none() {
            continue;
        }
        return Ok(process_token_evidence(&instance, token, pid, starttime));
    }
    Err(io::Error::other(format!(
        "no process in active application cgroup {instance:?} has argument {token:?}"
    )))
}

pub(crate) fn process_sandboxes(
    application: &str,
    namespace_pids: &[u32],
    limits: ResolvedResourceLimits,
    uid: u32,
    gid: u32,
) -> io::Result<Vec<ProcessSandbox>> {
    if namespace_pids.is_empty()
        || namespace_pids.len() > MAX_FIREFOX_CHILDREN
        || namespace_pids
            .iter()
            .any(|pid| *pid == 0 || namespace_pids.iter().filter(|seen| *seen == pid).count() != 1)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Firefox namespace PID set is empty, repeated, or over limit",
        ));
    }
    let (instance, directory, _) = active_instance(application, limits, uid, gid)?;
    let membership = membership_for_instance(&instance)?;
    let processes = read_control(&directory.join("cgroup.procs"))?;
    let mut sandboxes = Vec::new();
    for line in processes.lines() {
        let host_pid = line.parse::<u32>().map_err(|error| {
            io::Error::other(format!(
                "application cgroup {instance:?} has invalid process id {line:?}: {error}"
            ))
        })?;
        let proc_directory = PathBuf::from(format!("/proc/{host_pid}"));
        let status_path = proc_directory.join("status");
        let stat_path = proc_directory.join("stat");
        let Ok(starttime) = read_process_stat(&stat_path).and_then(|stat| process_starttime(&stat))
        else {
            continue;
        };
        let Ok(status) = read_process_status(&status_path) else {
            continue;
        };
        let Ok(initial) = sandbox_status(&status) else {
            continue;
        };
        let namespace_pid = initial.namespace_pid;
        if !namespace_pids.contains(&namespace_pid) {
            continue;
        }
        let membership_path = format!("/proc/{host_pid}/cgroup");
        let Ok(observed) = read_bounded(Path::new(&membership_path)) else {
            continue;
        };
        if require_membership_text(&observed, &membership).is_err() {
            continue;
        }
        let Ok(revalidated_status) = read_process_status(&status_path) else {
            continue;
        };
        let Ok(revalidated) = sandbox_status(&revalidated_status) else {
            continue;
        };
        let Ok(revalidated_starttime) =
            read_process_stat(&stat_path).and_then(|stat| process_starttime(&stat))
        else {
            continue;
        };
        let Some(revalidated) =
            revalidated_sandbox(initial, revalidated, starttime, revalidated_starttime)
        else {
            continue;
        };
        if sandboxes
            .iter()
            .any(|sandbox: &ProcessSandbox| sandbox.namespace_pid == namespace_pid)
        {
            return Err(io::Error::other(format!(
                "application cgroup {instance:?} repeats Firefox namespace PID {namespace_pid}"
            )));
        }
        sandboxes.push(revalidated);
    }
    if let Some(missing) = namespace_pids.iter().find(|pid| {
        !sandboxes
            .iter()
            .any(|sandbox| sandbox.namespace_pid == **pid)
    }) {
        return Err(io::Error::other(format!(
            "Firefox namespace PID {missing} is not a revalidated process in cgroup {instance:?}"
        )));
    }
    Ok(sandboxes)
}

fn read_process_status(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PROCESS_STATUS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROCESS_STATUS_BYTES {
        return Err(io::Error::other(format!(
            "{} exceeds {MAX_PROCESS_STATUS_BYTES} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::other(format!("{} is not UTF-8: {error}", path.display())))
}

fn read_process_stat(path: &Path) -> io::Result<String> {
    read_bounded(path)
}

fn process_starttime(stat: &str) -> io::Result<u64> {
    let fields = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| io::Error::other("process stat has no command-field terminator"))?;
    fields
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::other("process stat has no starttime"))?
        .parse::<u64>()
        .map_err(|error| io::Error::other(format!("process stat has invalid starttime: {error}")))
}

fn sandbox_status(status: &str) -> io::Result<ProcessSandbox> {
    Ok(ProcessSandbox {
        namespace_pid: namespace_process_id(status)?,
        no_new_privileges: decimal_status_field(status, "NoNewPrivs:")?,
        seccomp: decimal_status_field(status, "Seccomp:")?,
        filters: decimal_status_field(status, "Seccomp_filters:")?,
    })
}

fn revalidated_sandbox(
    initial: ProcessSandbox,
    revalidated: ProcessSandbox,
    starttime: u64,
    revalidated_starttime: u64,
) -> Option<ProcessSandbox> {
    (initial.namespace_pid == revalidated.namespace_pid && starttime == revalidated_starttime)
        .then_some(revalidated)
}

fn namespace_process_id(status: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|value| value.split_ascii_whitespace().next_back())
        .ok_or_else(|| io::Error::other("process status has no terminal NSpid"))?
        .parse::<u32>()
        .map_err(|error| io::Error::other(format!("process status has invalid NSpid: {error}")))
}

fn decimal_status_field(status: &str, name: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .ok_or_else(|| io::Error::other(format!("process status omitted {name}")))?
        .trim()
        .parse::<u32>()
        .map_err(|error| io::Error::other(format!("process status has invalid {name} {error}")))
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
    write_control(
        &directory.join("cpu.max"),
        &format!("{} {}", limits.cpu_quota_usec, limits.cpu_period_usec),
    )?;
    require_configuration(directory, limits)
}

fn require_configuration(directory: &Path, limits: ResolvedResourceLimits) -> io::Result<()> {
    for (name, expected) in [
        ("memory.high", limits.memory_high_bytes.to_string()),
        ("memory.max", limits.memory_max_bytes.to_string()),
        ("memory.oom.group", "1".to_string()),
        ("pids.max", limits.pids_max.to_string()),
        (
            "cpu.max",
            format!("{} {}", limits.cpu_quota_usec, limits.cpu_period_usec),
        ),
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
        &["cpu", "memory", "pids"],
        "delegated cgroup controllers",
    )?;
    require_words(
        &read_control(&root.join("cgroup.subtree_control"))?,
        &["cpu", "memory", "pids"],
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
        cpu: parse_cpu_stat(&read_control(&directory.join("cpu.stat"))?)?,
    })
}

fn parse_cpu_stat(text: &str) -> io::Result<CpuStat> {
    let mut stat = CpuStat::default();
    let mut found_periods = false;
    let mut found_throttled = false;
    let mut found_throttled_usec = false;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let name = fields
            .next()
            .ok_or_else(|| io::Error::other("empty cpu.stat row"))?;
        let value: u64 = fields
            .next()
            .ok_or_else(|| io::Error::other(format!("cpu.stat {name} has no value")))?
            .parse()
            .map_err(|error| io::Error::other(format!("cpu.stat {name} is invalid: {error}")))?;
        if fields.next().is_some() {
            return Err(io::Error::other(format!(
                "cpu.stat {name} has trailing fields"
            )));
        }
        match name {
            "nr_periods" => {
                stat.periods = value;
                found_periods = true;
            }
            "nr_throttled" => {
                stat.throttled = value;
                found_throttled = true;
            }
            "throttled_usec" => {
                stat.throttled_usec = value;
                found_throttled_usec = true;
            }
            _ => {}
        }
    }
    if !(found_periods && found_throttled && found_throttled_usec) {
        return Err(io::Error::other(
            "cpu.stat lacks nr_periods, nr_throttled or throttled_usec",
        ));
    }
    Ok(stat)
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
    fn cpu_diagnostics_require_the_bandwidth_rows() {
        let parsed = parse_cpu_stat(
            "usage_usec 100\nuser_usec 60\nsystem_usec 40\nnr_periods 7\nnr_throttled 2\nthrottled_usec 15\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            CpuStat {
                periods: 7,
                throttled: 2,
                throttled_usec: 15,
            }
        );
        assert!(parse_cpu_stat("usage_usec 100\nnr_periods 7\nnr_throttled 2\n").is_err());
        assert!(parse_cpu_stat("nr_periods 7 extra\nnr_throttled 2\nthrottled_usec 15\n").is_err());
    }

    #[test]
    fn controller_words_are_tokens_not_substrings() {
        require_words("cpu memory pids", &["cpu", "memory", "pids"], "controllers").unwrap();
        assert!(require_words("cpu memoryish pids", &["memory"], "controllers").is_err());
    }

    #[test]
    fn process_argument_evidence_is_exact_and_bounded() {
        assert!(valid_process_token("-contentproc"));
        assert!(!valid_process_token(""));
        assert!(!valid_process_token("content proc"));
        assert!(!valid_process_token("bad\nargument"));
        assert!(!valid_process_token(
            &"x".repeat(MAX_PROCESS_TOKEN_BYTES + 1)
        ));
        let command = b"/app/lib/firefox/firefox\0-contentproc\0--channel=7\0";
        assert!(command_has_token(command, "-contentproc"));
        assert!(!command_has_token(command, "contentproc"));
        assert!(!command_has_token(command, "-content"));
    }

    #[test]
    fn transient_or_oversized_process_commands_are_skipped() {
        let path = std::env::temp_dir().join(format!(
            "td-jail-process-command-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::write(&path, b"firefox\0-contentproc\0").unwrap();
        assert_eq!(
            read_process_command(&path).unwrap(),
            b"firefox\0-contentproc\0"
        );
        fs::write(
            &path,
            vec![0u8; usize::try_from(MAX_PROCESS_CMDLINE_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(read_process_command(&path).is_none());
        fs::remove_file(&path).unwrap();
        assert!(read_process_command(&path).is_none());
    }

    #[test]
    fn namespace_pid_and_sandbox_rows_are_exact() {
        let status =
            "Name:\tfirefox\nNSpid:\t8123\t17\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t3\n";
        assert_eq!(namespace_process_id(status).unwrap(), 17);
        assert_eq!(decimal_status_field(status, "NoNewPrivs:").unwrap(), 1);
        assert_eq!(decimal_status_field(status, "Seccomp:").unwrap(), 2);
        assert_eq!(decimal_status_field(status, "Seccomp_filters:").unwrap(), 3);
        assert!(namespace_process_id(&status.replace("NSpid:", "Pidns:")).is_err());
        assert!(decimal_status_field(status, "NoNewPriv:").is_err());
    }

    #[test]
    fn stable_process_identity_ignores_dynamic_status_rows() {
        let before = "Name:\tfirefox\nNSpid:\t8123\t17\nVmRSS:\t100 kB\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t2\nvoluntary_ctxt_switches:\t9\n";
        let after = "Name:\tfirefox\nNSpid:\t8123\t17\nVmRSS:\t180 kB\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t3\nvoluntary_ctxt_switches:\t12\n";
        assert_ne!(before, after);
        let initial = sandbox_status(before).unwrap();
        let revalidated = sandbox_status(after).unwrap();
        assert_eq!(
            revalidated_sandbox(initial, revalidated, 4242, 4242),
            Some(revalidated)
        );
        assert_eq!(revalidated.filters, 3);
        assert_eq!(
            revalidated_sandbox(
                initial,
                ProcessSandbox {
                    namespace_pid: 18,
                    ..revalidated
                },
                4242,
                4242,
            ),
            None
        );
        assert_eq!(revalidated_sandbox(initial, revalidated, 4242, 4243), None);

        let stat = "8123 (firefox child) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20";
        assert_eq!(process_starttime(stat).unwrap(), 4242);
        assert!(process_starttime("8123 firefox S 1 2").is_err());
        let path = std::env::temp_dir().join(format!(
            "td-jail-process-starttime-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::write(&path, stat).unwrap();
        assert_eq!(revalidated_process_starttime(&path, 4242), Some(4242));
        assert_eq!(revalidated_process_starttime(&path, 4243), None);
        fs::remove_file(path).unwrap();
        assert_eq!(
            process_token_evidence("firefox-abcd", "--marionette", 8123, 4242),
            "TD-JAIL-PROCESS-TOKEN-OK instance=firefox-abcd token=--marionette \
             pid=8123 starttime=4242"
        );
    }
}

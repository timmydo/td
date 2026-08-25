use crate::{
    authority::{
        self, decode_mountinfo_path, mount_identity_for_path, path_is_same_or_child,
        paths_overlap,
        FilesystemGrant, FilesystemSourceKind, LaunchPlan, ResolvedResourceLimits,
    },
    cgroup, seccomp, sys,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const PROBE_ARG: &str = "--probe-transition";
pub const RESOURCE_PROBE_ARG: &str = "--probe-resource-caps";
const FILTER_ARG: &str = "--internal-write-seccomp-filter";
const APPLICATION_SESSION_ARG: &str = "--internal-application-session";
const CGROUP_CLEANUP_ARG: &str = "--internal-cgroup-cleanup";
const CGROUP_CLEANUP_WATCH_ARG: &str = "--internal-cgroup-cleanup-watch";
const CGROUP_CLEANUP_READY: [u8; 1] = [1];
const STAGE2_ARG: &str = "--internal-stage-2";
const STAGE2_PROBE_ARG: &str = "--probe";
const STAGE2_LAUNCH_ARG: &str = "--launch";
const STAGE2_ENVIRONMENT_ARG: &str = "--environment";
const STAGE2_FILESYSTEMS_ARG: &str = "--filesystems";
const STAGE2_RESOURCES_ARG: &str = "--resources";
const STAGE2_ARGUMENTS_ARG: &str = "--arguments";
const NO_CGROUP_MEMBERSHIP: &str = "none";
const REAPER_CHILD_ARG: &str = "--internal-reaper-child";
const REAPER_ORPHAN_ARG: &str = "--internal-reaper-orphan";
const SURVIVOR_CHILD_ARG: &str = "--internal-survivor-child";
const SURVIVOR_ORPHAN_ARG: &str = "--internal-survivor-orphan";
pub const TRANSITION_MARKER: &str = "TD-JAIL-TRANSITION-OK";
pub const HOST_DEGRADATION_CGROUP: &str =
    "TD-JAIL-HOST-DEGRADATION aggregate-memory-and-task-caps=unenforced reason=no-delegated-cgroup";
pub const HOST_DEGRADATION_WAYLAND: &str =
    "TD-JAIL-HOST-DEGRADATION wayland-global-filter=unenforced reason=direct-host-socket";
const STAGE2_MARKER: &str = "TD-JAIL-STAGE2-OK";
const TOKEN_LEN: usize = 32;
/// Bytes of randomness in an instance name's suffix.
///
/// Sixteen hex characters onto a name of at most 32 leaves 49, inside the
/// broker's 64-byte instance-name ceiling. It is a uniqueness suffix and not a
/// secret — the broker's own token is the secret — so the width only has to
/// make a collision between two live launches implausible.
const INSTANCE_SUFFIX_LEN: usize = 8;
const TEST_LEAK_ENV: &str = "TD_JAIL_TEST_LEAK_FD";
const MAX_CAPABILITY: u32 = 63;
const SYS_ADMIN_MASK: u64 = 1_u64 << sys::CAP_SYS_ADMIN;
const SETPCAP_MASK: u64 = 1_u64 << sys::CAP_SETPCAP;

const SCRATCH_ROOT: &str = "/tmp";
const NEW_ROOT: &str = "/tmp/td-jail-root";
const PUT_OLD: &str = "/tmp/td-jail-root/oldroot";
const OLD_ROOT: &str = "/oldroot";
const REAPER_PROBE_PATH: &str = "/tmp/td-jail-reaper-probe";
const REAPER_TIMEOUT: Duration = Duration::from_secs(2);
const REAPER_POLL: Duration = Duration::from_millis(5);
const SURVIVOR_TERM_TIMEOUT: Duration = Duration::from_secs(2);
const SURVIVOR_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const SURVIVOR_PROBE_LIFETIME: Duration = Duration::from_secs(30);
const STAGE2_OUTPUT_LIMIT: usize = 4096;
const WRITE_PROBE_PREFIX: &str = ".td-jail-write-probe-";

const DEVICE_NODES: &[(&str, u64, u64)] = &[
    ("null", 1, 3),
    ("zero", 1, 5),
    ("full", 1, 7),
    ("random", 1, 8),
    ("urandom", 1, 9),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    uid: u32,
    gid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LaunchIdentityMap {
    inside: Identity,
    outside: Identity,
}

#[derive(Debug, Eq, PartialEq)]
struct NamespaceSnapshot {
    user: PathBuf,
    mount: PathBuf,
    pid: PathBuf,
    uts: PathBuf,
    network: PathBuf,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct SurvivorReap {
    count: u64,
    sole_child: Option<(i32, i32)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainOutcome {
    Drained,
    DeadlineExpired,
}

impl SurvivorReap {
    fn record(&mut self, pid: i32, status: i32) {
        self.sole_child = if self.count == 0 {
            Some((pid, status))
        } else {
            None
        };
        self.count = self.count.saturating_add(1);
    }
}

impl NamespaceSnapshot {
    fn read() -> io::Result<Self> {
        Ok(Self {
            user: fs::read_link("/proc/self/ns/user")?,
            mount: fs::read_link("/proc/self/ns/mnt")?,
            pid: fs::read_link("/proc/self/ns/pid")?,
            uts: fs::read_link("/proc/self/ns/uts")?,
            network: fs::read_link("/proc/self/ns/net")?,
        })
    }

    fn require_all_changed(&self, before: &Self) -> io::Result<()> {
        for (name, old, new) in [
            ("user", &before.user, &self.user),
            ("mount", &before.mount, &self.mount),
            ("uts", &before.uts, &self.uts),
            ("network", &before.network, &self.network),
        ] {
            if old == new {
                return Err(io::Error::other(format!(
                    "unshare reported success but the {name} namespace did not change"
                )));
            }
        }
        Ok(())
    }
}

fn require_child_pid_namespace_changed(before: &NamespaceSnapshot, child: u32) -> io::Result<()> {
    let path = format!("/proc/{child}/ns/pid");
    let current = fs::read_link(&path)
        .map_err(|e| io::Error::other(format!("read stage-2 PID namespace at {path}: {e}")))?;
    if current == before.pid {
        return Err(io::Error::other(
            "stage 2 remained in stage 1's PID namespace",
        ));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub enum Mode {
    Probe,
    ResourceProbe {
        application: String,
    },
    WriteFilter,
    ApplicationSession {
        parent: u32,
        argv0: OsString,
        arguments: Vec<OsString>,
    },
    CgroupCleanupBootstrap {
        membership: String,
    },
    CgroupCleanupWatcher {
        membership: String,
    },
    Stage2 {
        token: [u8; TOKEN_LEN],
        identity: Identity,
        outside_identity: Identity,
        action: Stage2Action,
    },
    ReaperChild,
    ReaperOrphan,
    SurvivorChild,
    SurvivorOrphan,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Stage2Action {
    Probe,
    Launch {
        entry: String,
        environment: Vec<(OsString, OsString)>,
        filesystems: Vec<Stage2Filesystem>,
        resources: ResolvedResourceLimits,
        cgroup_membership: String,
        arguments: Vec<OsString>,
    },
}

#[derive(Clone, Copy)]
struct Stage2ResourceBinding<'a> {
    limits: ResolvedResourceLimits,
    membership: &'a str,
}

struct CgroupCleanup {
    child: Option<Child>,
    keepalive: Option<io::PipeWriter>,
}

struct ManagedCgroup {
    instance: Option<cgroup::Instance>,
    cleanup: Option<CgroupCleanup>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stage2Filesystem {
    target: PathBuf,
    read_only: bool,
    source_kind: FilesystemSourceKind,
}

pub fn parse_mode<I>(mut args: I) -> io::Result<Mode>
where
    I: Iterator<Item = OsString>,
{
    let mode = args.next().ok_or_else(usage_error)?;
    if mode == PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::Probe);
    }
    if mode == RESOURCE_PROBE_ARG {
        let application = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        authority::validate_application_name(&application)?;
        return Ok(Mode::ResourceProbe { application });
    }
    if mode == FILTER_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::WriteFilter);
    }
    if mode == APPLICATION_SESSION_ARG {
        let parent = parse_positive_pid(args.next())?;
        let argv0 = args.next().ok_or_else(usage_error)?;
        return Ok(Mode::ApplicationSession {
            parent,
            argv0,
            arguments: args.collect(),
        });
    }
    if mode == CGROUP_CLEANUP_ARG {
        let membership = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        cgroup::validate_expected_membership(&membership)?;
        return Ok(Mode::CgroupCleanupBootstrap { membership });
    }
    if mode == CGROUP_CLEANUP_WATCH_ARG {
        let membership = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        cgroup::validate_expected_membership(&membership)?;
        return Ok(Mode::CgroupCleanupWatcher { membership });
    }
    if mode == STAGE2_ARG {
        let encoded = args.next().ok_or_else(usage_error)?;
        let uid = parse_id(args.next(), "uid")?;
        let gid = parse_id(args.next(), "gid")?;
        let outside_uid = parse_id(args.next(), "outside uid")?;
        let outside_gid = parse_id(args.next(), "outside gid")?;
        let action = parse_stage2_action(&mut args, uid)?;
        return Ok(Mode::Stage2 {
            token: decode_token(&encoded)?,
            identity: Identity { uid, gid },
            outside_identity: Identity {
                uid: outside_uid,
                gid: outside_gid,
            },
            action,
        });
    }
    if mode == REAPER_CHILD_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::ReaperChild);
    }
    if mode == REAPER_ORPHAN_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::ReaperOrphan);
    }
    if mode == SURVIVOR_CHILD_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::SurvivorChild);
    }
    if mode == SURVIVOR_ORPHAN_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::SurvivorOrphan);
    }
    Err(usage_error())
}

fn parse_stage2_action<I>(args: &mut I, uid: u32) -> io::Result<Stage2Action>
where
    I: Iterator<Item = OsString>,
{
    let action = args.next().ok_or_else(usage_error)?;
    if action == STAGE2_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Stage2Action::Probe);
    }
    if action == STAGE2_LAUNCH_ARG {
        let entry = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        authority::validate_entry(&entry)?;
        if args.next().as_deref() != Some(STAGE2_ENVIRONMENT_ARG.as_ref()) {
            return Err(usage_error());
        }
        let count = parse_count(args.next(), "environment count")?;
        if count > authority::MAX_ENVIRONMENT_ENTRIES {
            return Err(usage_error());
        }
        let mut environment = Vec::with_capacity(count);
        for _ in 0..count {
            let key = args.next().ok_or_else(usage_error)?;
            let value = args.next().ok_or_else(usage_error)?;
            environment.push((key, value));
        }
        authority::validate_environment_list(&environment, uid)?;
        if args.next().as_deref() != Some(STAGE2_FILESYSTEMS_ARG.as_ref()) {
            return Err(usage_error());
        }
        let filesystem_count = parse_count(args.next(), "filesystem count")?;
        if filesystem_count > crate::permissions::MAX_FILESYSTEM_ENTRIES {
            return Err(usage_error());
        }
        let mut filesystems = Vec::with_capacity(filesystem_count);
        let mut previous: Option<PathBuf> = None;
        for _ in 0..filesystem_count {
            let target = args
                .next()
                .ok_or_else(usage_error)?
                .into_string()
                .map_err(|_| usage_error())?;
            let target = PathBuf::from(target);
            authority::validate_filesystem_target(&target)?;
            if previous.as_ref().is_some_and(|prior| prior >= &target) {
                return Err(usage_error());
            }
            if filesystems
                .iter()
                .any(|grant: &Stage2Filesystem| paths_overlap(&grant.target, &target))
            {
                return Err(usage_error());
            }
            let mode = args.next().ok_or_else(usage_error)?;
            let (read_only, source_kind) = match mode.to_str() {
                Some("ro-dir") => (true, FilesystemSourceKind::Directory),
                Some("rw-dir") => (false, FilesystemSourceKind::Directory),
                Some("ro-file") => (true, FilesystemSourceKind::File),
                Some("rw-file") => (false, FilesystemSourceKind::File),
                _ => return Err(usage_error()),
            };
            previous = Some(target.clone());
            filesystems.push(Stage2Filesystem {
                target,
                read_only,
                source_kind,
            });
        }
        if args.next().as_deref() != Some(STAGE2_RESOURCES_ARG.as_ref()) {
            return Err(usage_error());
        }
        let memory_high_bytes = parse_u64(args.next(), "memory-high")?;
        let memory_max_bytes = parse_u64(args.next(), "memory-max")?;
        let pids_max = parse_id(args.next(), "pids-max")?;
        let resources =
            ResolvedResourceLimits::from_stage2(memory_high_bytes, memory_max_bytes, pids_max)?;
        let cgroup_membership = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if cgroup_membership != NO_CGROUP_MEMBERSHIP {
            cgroup::validate_expected_membership(&cgroup_membership)?;
        }
        if args.next().as_deref() != Some(STAGE2_ARGUMENTS_ARG.as_ref()) {
            return Err(usage_error());
        }
        return Ok(Stage2Action::Launch {
            entry,
            environment,
            filesystems,
            resources,
            cgroup_membership,
            arguments: authority::collect_arguments(args)?,
        });
    }
    Err(usage_error())
}

fn parse_u64(value: Option<OsString>, name: &str) -> io::Result<u64> {
    value
        .ok_or_else(usage_error)?
        .to_str()
        .ok_or_else(usage_error)?
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stage-2 {name}: {error}"),
            )
        })
}

fn parse_id(value: Option<OsString>, name: &str) -> io::Result<u32> {
    value
        .ok_or_else(usage_error)?
        .to_str()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("stage-2 {name} is not UTF-8"),
            )
        })?
        .parse()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stage-2 {name}: {e}"),
            )
        })
}

fn parse_positive_pid(value: Option<OsString>) -> io::Result<u32> {
    let value = value.ok_or_else(usage_error)?;
    let value = value.to_str().ok_or_else(usage_error)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(usage_error());
    }
    let pid = value.parse::<u32>().map_err(|_| usage_error())?;
    if pid == 0 {
        return Err(usage_error());
    }
    Ok(pid)
}

fn stage2_launch_arguments(
    token: &[u8; TOKEN_LEN],
    identity_map: LaunchIdentityMap,
    entry: &str,
    environment: &[(OsString, OsString)],
    filesystems: &[FilesystemGrant],
    resources: Stage2ResourceBinding<'_>,
    arguments: &[OsString],
) -> Vec<OsString> {
    let mut stage2 = Vec::with_capacity(
        16usize
            .saturating_add(environment.len().saturating_mul(2))
            .saturating_add(filesystems.len().saturating_mul(2))
            .saturating_add(arguments.len()),
    );
    stage2.extend([
        OsString::from(STAGE2_ARG),
        OsString::from(encode_token(token)),
        OsString::from(identity_map.inside.uid.to_string()),
        OsString::from(identity_map.inside.gid.to_string()),
        OsString::from(identity_map.outside.uid.to_string()),
        OsString::from(identity_map.outside.gid.to_string()),
        OsString::from(STAGE2_LAUNCH_ARG),
        OsString::from(entry),
        OsString::from(STAGE2_ENVIRONMENT_ARG),
        OsString::from(environment.len().to_string()),
    ]);
    stage2.extend(
        environment
            .iter()
            .flat_map(|(key, value)| [key.clone(), value.clone()]),
    );
    stage2.push(OsString::from(STAGE2_FILESYSTEMS_ARG));
    stage2.push(OsString::from(filesystems.len().to_string()));
    for filesystem in filesystems {
        stage2.push(filesystem.target.as_os_str().to_os_string());
        let mode = match (filesystem.read_only, filesystem.source_kind) {
            (true, FilesystemSourceKind::Directory) => "ro-dir",
            (false, FilesystemSourceKind::Directory) => "rw-dir",
            (true, FilesystemSourceKind::File) => "ro-file",
            (false, FilesystemSourceKind::File) => "rw-file",
        };
        stage2.push(OsString::from(mode));
    }
    stage2.extend([
        OsString::from(STAGE2_RESOURCES_ARG),
        OsString::from(resources.limits.memory_high_bytes.to_string()),
        OsString::from(resources.limits.memory_max_bytes.to_string()),
        OsString::from(resources.limits.pids_max.to_string()),
        OsString::from(resources.membership),
    ]);
    stage2.push(OsString::from(STAGE2_ARGUMENTS_ARG));
    stage2.extend(arguments.iter().cloned());
    stage2
}

fn parse_count(value: Option<OsString>, name: &str) -> io::Result<usize> {
    value
        .ok_or_else(usage_error)?
        .to_str()
        .ok_or_else(usage_error)?
        .parse()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stage-2 {name}: {e}"),
            )
        })
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "bare td-jail accepts only --probe-transition or --probe-resource-caps NAME; installed applications are selected by argv[0]",
    )
}

fn current_identity() -> io::Result<Identity> {
    let status = fs::read_to_string("/proc/self/status")?;
    Ok(Identity {
        uid: effective_id(&status, "Uid:")?,
        gid: effective_id(&status, "Gid:")?,
    })
}

fn capability_row(status: &str, key: &str) -> io::Result<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .ok_or_else(|| io::Error::other(format!("/proc/self/status has no {key} row")))?
        .trim();
    u64::from_str_radix(value, 16)
        .map_err(|e| io::Error::other(format!("invalid /proc/self/status {key}: {e}")))
}

fn effective_id(status: &str, key: &str) -> io::Result<u32> {
    let fields = status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .ok_or_else(|| io::Error::other(format!("/proc/self/status has no {key} row")))?;
    fields
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other(format!("/proc/self/status {key} has no effective id")))?
        .parse()
        .map_err(|e| io::Error::other(format!("invalid /proc/self/status {key}: {e}")))
}

fn decimal_status_row(status: &str, key: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .ok_or_else(|| io::Error::other(format!("/proc/self/status has no {key} row")))?
        .trim()
        .parse::<u32>()
        .map_err(|e| io::Error::other(format!("invalid /proc/self/status {key}: {e}")))
}

fn install_identity_maps(inside: Identity, outside: Identity) -> io::Result<()> {
    fs::write("/proc/self/setgroups", "deny\n")?;
    fs::write(
        "/proc/self/uid_map",
        format!("{} {} 1\n", inside.uid, outside.uid),
    )?;
    fs::write(
        "/proc/self/gid_map",
        format!("{} {} 1\n", inside.gid, outside.gid),
    )?;
    require_single_map("/proc/self/uid_map", inside.uid, Some(outside.uid))?;
    require_single_map("/proc/self/gid_map", inside.gid, Some(outside.gid))
}

fn install_launch_identity_maps(
    inside: Identity,
    outside: Identity,
    host_mode: bool,
) -> io::Result<()> {
    let result = (|| {
        install_identity_maps(inside, outside)?;
        if current_identity()? != inside {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "application identity map did not produce the configured inside identity",
            ));
        }
        Ok(())
    })();
    result.map_err(|error| {
        if host_mode {
            io::Error::new(
                error.kind(),
                format!("host mode requires user-namespace identity mapping: {error}"),
            )
        } else {
            error
        }
    })
}

fn require_single_map(path: &str, inside: u32, outside: Option<u32>) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    validate_single_map(path, &text, inside, outside)
}

fn validate_single_map(
    path: &str,
    text: &str,
    inside: u32,
    outside: Option<u32>,
) -> io::Result<()> {
    let mut rows = text.lines().filter(|line| !line.trim().is_empty());
    let row = rows
        .next()
        .ok_or_else(|| io::Error::other(format!("{path} is empty after write")))?;
    if rows.next().is_some() {
        return Err(io::Error::other(format!(
            "{path} contains more than the one identity mapping td-jail wrote"
        )));
    }
    let values = row
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("invalid {path} readback: {e}")))?;
    let valid = values.len() == 3
        && values.first() == Some(&inside)
        && values.get(2) == Some(&1)
        && outside.is_none_or(|expected| values.get(1) == Some(&expected));
    if !valid {
        return Err(io::Error::other(format!(
            "{path} readback does not match the identity map td-jail wrote"
        )));
    }
    Ok(())
}

fn random_token() -> io::Result<[u8; TOKEN_LEN]> {
    let mut token = [0_u8; TOKEN_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut token)?;
    Ok(token)
}

/// A name for one launch of one application.
///
/// §D's instance name identifies a LAUNCH, not an application: two windows of
/// the same program are two instances, and the broker refuses a name that is
/// already registered. So it carries a random suffix rather than the pid — a
/// pid is unique among live processes and is reissued, which is the property
/// this module spends its time not relying on elsewhere.
///
/// The proof token is deliberately not reused here. That token is a one-shot
/// secret that authenticates stage 2 through the pipe; an instance name is an
/// identifier, compared by the broker and echoed back in its refusals — the
/// duplicate-registration error quotes it. Nothing today puts an instance name
/// on the wire to another peer, but a name goes where names go, and a secret
/// spent as one stops being a secret at whichever of those places comes first.
fn instance_name(application: &str) -> io::Result<String> {
    let mut suffix = [0_u8; INSTANCE_SUFFIX_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut suffix)?;
    let mut name = String::with_capacity(application.len() + 1 + suffix.len() * 2);
    name.push_str(application);
    name.push('-');
    for byte in suffix {
        name.push(encode_nibble(byte >> 4));
        name.push(encode_nibble(byte & 0x0f));
    }
    Ok(name)
}

fn encode_token(token: &[u8; TOKEN_LEN]) -> String {
    let mut encoded = String::with_capacity(TOKEN_LEN * 2);
    for byte in token {
        encoded.push(encode_nibble(byte >> 4));
        encoded.push(encode_nibble(byte & 0x0f));
    }
    encoded
}

fn encode_nibble(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        _ => char::from(b'a' + nibble - 10),
    }
}

fn decode_token(encoded: &OsString) -> io::Result<[u8; TOKEN_LEN]> {
    let bytes = encoded
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stage-2 token is not UTF-8"))?
        .as_bytes();
    if bytes.len() != TOKEN_LEN * 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stage-2 token has the wrong length",
        ));
    }
    let mut token = [0_u8; TOKEN_LEN];
    for (index, slot) in token.iter_mut().enumerate() {
        let offset = index * 2;
        let high = decode_nibble(bytes.get(offset).copied())?;
        let low = decode_nibble(bytes.get(offset + 1).copied())?;
        *slot = (high << 4) | low;
    }
    Ok(token)
}

fn decode_nibble(byte: Option<u8>) -> io::Result<u8> {
    match byte {
        Some(value @ b'0'..=b'9') => Ok(value - b'0'),
        Some(value @ b'a'..=b'f') => Ok(value - b'a' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stage-2 token is not lowercase hexadecimal",
        )),
    }
}

fn tokens_equal(left: &[u8; TOKEN_LEN], right: &[u8; TOKEN_LEN]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}

fn cstring(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| io::Error::other(format!("path contains NUL: {value:?}")))
}

fn close_inherited_descriptors(preserve: Option<u32>) -> io::Result<()> {
    let mut descriptors = Vec::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("/proc/self/fd contained a non-UTF-8 descriptor name"))?;
        let descriptor = name.parse::<u32>().map_err(|e| {
            io::Error::other(format!(
                "/proc/self/fd contained nonnumeric entry {name:?}: {e}"
            ))
        })?;
        if descriptor > 2 && Some(descriptor) != preserve {
            descriptors.push(descriptor);
        }
    }
    descriptors.sort_unstable();
    descriptors.dedup();
    for descriptor in descriptors {
        if let Err(error) = sys::close(descriptor) {
            if error.raw_os_error() != Some(9) {
                return Err(io::Error::other(format!(
                    "close inherited descriptor {descriptor}: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn install_test_leak_if_requested() -> io::Result<Option<u32>> {
    let Some(value) = std::env::var_os(TEST_LEAK_ENV) else {
        return Ok(None);
    };
    if value != "1" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{TEST_LEAK_ENV} must be 1 when present"),
        ));
    }
    let raw = fs::File::open("/proc/self/status")?.into_raw_fd();
    let descriptor = u32::try_from(raw)
        .map_err(|e| io::Error::other(format!("test leak descriptor is invalid: {e}")))?;
    if descriptor <= 2 || fs::symlink_metadata(format!("/proc/self/fd/{descriptor}")).is_err() {
        return Err(io::Error::other(
            "test leak did not create a live descriptor above stderr",
        ));
    }
    Ok(Some(descriptor))
}

fn require_descriptor_closed(descriptor: u32) -> io::Result<()> {
    match fs::symlink_metadata(format!("/proc/self/fd/{descriptor}")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::other(format!(
            "descriptor closure sweep left test descriptor {descriptor} open"
        ))),
    }
}

fn require_only_stdio_descriptors() -> io::Result<()> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("stage-2 descriptor name is not UTF-8"))?;
        let descriptor = name.parse::<u32>().map_err(|e| {
            io::Error::other(format!("stage-2 descriptor name {name:?} is invalid: {e}"))
        })?;
        if descriptor > 2 {
            candidates.push(descriptor);
        }
    }
    for descriptor in candidates {
        match fs::symlink_metadata(format!("/proc/self/fd/{descriptor}")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("stage 2 inherited descriptor {descriptor} above stderr"),
                ));
            }
        }
    }
    Ok(())
}

fn create_dir(path: &str, mode: u32) -> io::Result<()> {
    fs::create_dir(path)
        .map_err(|error| io::Error::new(error.kind(), format!("create {path}: {error}")))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        io::Error::new(error.kind(), format!("set mode {mode:#o} on {path}: {error}"))
    })
}

fn mount_tmpfs(path: &str, flags: usize, data: &str) -> io::Result<()> {
    let tmpfs = cstring("tmpfs")?;
    let target = cstring(path)?;
    let data = cstring(data)?;
    sys::mount(Some(&tmpfs), &target, Some(&tmpfs), flags, Some(&data))
        .map_err(|e| io::Error::other(format!("mount tmpfs at {path}: {e}")))
}

fn mount_bind_read_only(source: &str, target: &str) -> io::Result<()> {
    mount_bind(Path::new(source), target)?;
    remount_read_only(target, sys::MS_BIND | sys::MS_NOSUID | sys::MS_NOEXEC)
}

fn mount_application_tree(source: &Path, target: &str) -> io::Result<()> {
    mount_bind(source, target)?;
    remount_read_only(target, sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV)
}

fn mount_private_bind(source: &Path, target: &str, read_only: bool) -> io::Result<()> {
    mount_bind(source, target)?;
    let target_c = cstring(target)?;
    let mut flags =
        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC;
    if read_only {
        flags |= sys::MS_RDONLY;
    }
    sys::mount(None, &target_c, None, flags, None)
        .map_err(|e| io::Error::other(format!("remount {target} as a private bind: {e}")))
}

fn mount_bind(source: &Path, target: &str) -> io::Result<()> {
    let source = source.to_str().ok_or_else(|| {
        io::Error::other(format!("mount source is not UTF-8: {}", source.display()))
    })?;
    let source_c = cstring(source)?;
    let target_c = cstring(target)?;
    sys::mount(Some(&source_c), &target_c, None, sys::MS_BIND, None)
        .map_err(|e| io::Error::other(format!("bind {source} at {target}: {e}")))
}

fn mount_filesystem_grant(grant: &FilesystemGrant) -> io::Result<()> {
    authority::validate_filesystem_target(&grant.target)?;
    require_filesystem_source_identity(grant)?;
    let target = prepare_filesystem_target(&grant.target, grant.source_kind)?;
    let source = grant.source.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "filesystem grant source is not UTF-8: {}",
            grant.source.display()
        ))
    })?;
    let target_text = target.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "filesystem grant target is not UTF-8: {}",
            target.display()
        ))
    })?;
    let source_c = cstring(source)?;
    let target_c = cstring(target_text)?;
    let flags = sys::MS_BIND
        | if grant.source_kind == FilesystemSourceKind::Directory {
            sys::MS_REC
        } else {
            0
        };
    sys::mount(Some(&source_c), &target_c, None, flags, None).map_err(|error| {
        io::Error::other(format!(
            "bind filesystem grant {} at {}: {error}",
            grant.source.display(),
            grant.target.display()
        ))
    })?;
    apply_grant_mount_policy(&target, grant.read_only)?;
    let target_metadata = fs::symlink_metadata(&target)?;
    if target_metadata.dev() != grant.source_device
        || target_metadata.ino() != grant.source_inode
        || (target_metadata.file_type().is_dir()
            != (grant.source_kind == FilesystemSourceKind::Directory))
        || (target_metadata.file_type().is_file()
            != (grant.source_kind == FilesystemSourceKind::File))
        || (grant.source_kind == FilesystemSourceKind::File && target_metadata.nlink() != 1)
    {
        return Err(io::Error::other(format!(
            "filesystem grant target {} does not retain the authenticated source identity",
            grant.target.display()
        )));
    }
    require_filesystem_source_identity(grant)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_bind_source(&mountinfo, &grant.source, &target)?;
    require_grant_mount_policy(
        &mountinfo,
        &Stage2Filesystem {
            target,
            read_only: grant.read_only,
            source_kind: grant.source_kind,
        },
    )
}

fn require_filesystem_source_identity(grant: &FilesystemGrant) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&grant.source)?;
    let kind_matches = match grant.source_kind {
        FilesystemSourceKind::Directory => metadata.file_type().is_dir(),
        FilesystemSourceKind::File => metadata.file_type().is_file(),
    };
    if !kind_matches
        || metadata.dev() != grant.source_device
        || metadata.ino() != grant.source_inode
        || (grant.source_kind == FilesystemSourceKind::File && metadata.nlink() != 1)
    {
        return Err(io::Error::other(format!(
            "filesystem grant source {} changed after authority resolution",
            grant.source.display()
        )));
    }
    Ok(())
}

fn prepare_filesystem_target(
    target: &Path,
    source_kind: FilesystemSourceKind,
) -> io::Result<PathBuf> {
    let relative = target
        .strip_prefix("/")
        .map_err(|_| io::Error::other("filesystem target is not absolute"))?;
    let mut current = PathBuf::from(NEW_ROOT);
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let final_component = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let expected_directory =
                    !final_component || source_kind == FilesystemSourceKind::Directory;
                if (expected_directory && !metadata.file_type().is_dir())
                    || (!expected_directory && !metadata.file_type().is_file())
                {
                    return Err(io::Error::other(format!(
                        "filesystem grant target component {} has the wrong type",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if final_component && source_kind == FilesystemSourceKind::File {
                    drop(
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&current)?,
                    );
                } else {
                    create_dir(
                        current.to_str().ok_or_else(|| {
                            io::Error::other("filesystem target component is not UTF-8")
                        })?,
                        if final_component { 0o700 } else { 0o755 },
                    )?;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

#[derive(Clone, Debug)]
struct MountPolicyRow {
    mountpoint: PathBuf,
    read_only: bool,
    options: BTreeSet<String>,
}

fn grant_mount_rows(mountinfo: &str, target: &Path) -> io::Result<Vec<MountPolicyRow>> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for line in mountinfo.lines() {
        let left = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?
            .0;
        let mut fields = left.split_whitespace();
        let mountpoint = fields
            .nth(4)
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        let options = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no mount options"))?
            .split(',')
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let mountpoint = decode_mountinfo_path(mountpoint)?;
        if !path_is_same_or_child(&mountpoint, target) {
            continue;
        }
        if !seen.insert(mountpoint.clone()) {
            return Err(io::Error::other(format!(
                "mountinfo repeats filesystem grant mountpoint {}",
                mountpoint.display()
            )));
        }
        rows.push(MountPolicyRow {
            read_only: options.contains("ro"),
            mountpoint,
            options,
        });
    }
    if !seen.contains(target) {
        return Err(io::Error::other(format!(
            "mountinfo contains no filesystem grant target {}",
            target.display()
        )));
    }
    Ok(rows)
}

fn sort_grant_mount_rows(rows: &mut [MountPolicyRow]) {
    rows.sort_by(|left, right| {
        right
            .mountpoint
            .components()
            .count()
            .cmp(&left.mountpoint.components().count())
            .then_with(|| right.mountpoint.cmp(&left.mountpoint))
    });
}

fn apply_grant_mount_policy(target: &Path, read_only: bool) -> io::Result<()> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let mut rows = grant_mount_rows(&mountinfo, target)?;
    sort_grant_mount_rows(&mut rows);
    for row in rows {
        let path = row.mountpoint.to_str().ok_or_else(|| {
            io::Error::other(format!(
                "filesystem grant mountpoint is not UTF-8: {}",
                row.mountpoint.display()
            ))
        })?;
        let target_c = cstring(path)?;
        let flags = grant_mount_policy_flags(read_only || row.read_only);
        sys::mount(None, &target_c, None, flags, None).map_err(|error| {
            io::Error::other(format!(
                "apply filesystem grant policy at {}: {error}",
                row.mountpoint.display()
            ))
        })?;
    }
    Ok(())
}

fn grant_mount_policy_flags(read_only: bool) -> usize {
    let mut flags =
        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC;
    if read_only {
        flags |= sys::MS_RDONLY;
    }
    flags
}

fn require_grant_mount_policy(mountinfo: &str, filesystem: &Stage2Filesystem) -> io::Result<()> {
    let rows = grant_mount_rows(mountinfo, &filesystem.target)?;
    for row in rows {
        for required in ["nosuid", "nodev", "noexec"] {
            if !row.options.contains(required) {
                return Err(io::Error::other(format!(
                    "filesystem grant mount {} lacks {required}",
                    row.mountpoint.display()
                )));
            }
        }
        if filesystem.read_only && !row.options.contains("ro") {
            return Err(io::Error::other(format!(
                "read-only filesystem grant has writable mount {}",
                row.mountpoint.display()
            )));
        }
        if !filesystem.read_only
            && row.mountpoint == filesystem.target
            && !row.options.contains("rw")
        {
            return Err(io::Error::other(format!(
                "read-write filesystem grant target {} is not writable",
                row.mountpoint.display()
            )));
        }
    }
    Ok(())
}

fn require_writable_file(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .map(drop)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "open read-write filesystem grant {} for writing: {error}",
                    path.display()
                ),
            )
        })
}

fn remount_read_only(path: &str, flags: usize) -> io::Result<()> {
    let target = cstring(path)?;
    sys::mount(
        None,
        &target,
        None,
        sys::MS_REMOUNT | sys::MS_RDONLY | flags,
        None,
    )
    .map_err(|e| io::Error::other(format!("remount {path} read-only: {e}")))
}

fn prepare_mount_plan(
    identity: Identity,
    executable: &Path,
    application: Option<&LaunchPlan>,
) -> io::Result<()> {
    let root = cstring("/")?;
    sys::mount(None, &root, None, sys::MS_REC | sys::MS_PRIVATE, None)
        .map_err(|e| io::Error::other(format!("make mount tree private: {e}")))?;

    mount_tmpfs(
        SCRATCH_ROOT,
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        "mode=0700",
    )?;
    create_dir(NEW_ROOT, 0o755)?;
    mount_tmpfs(NEW_ROOT, sys::MS_NOSUID | sys::MS_NODEV, "mode=0755")?;

    for (path, mode) in [
        (format!("{NEW_ROOT}/dev"), 0o755),
        (format!("{NEW_ROOT}/proc"), 0o555),
        (format!("{NEW_ROOT}/tmp"), 0o1777),
        (format!("{NEW_ROOT}/var"), 0o755),
        (PUT_OLD.to_string(), 0o700),
    ] {
        create_dir(&path, mode)?;
    }
    create_dir(&format!("{NEW_ROOT}/var/tmp"), 0o1777)?;

    if application.is_some() {
        for (path, mode) in [
            (format!("{NEW_ROOT}/app"), 0o555),
            (format!("{NEW_ROOT}/usr"), 0o555),
            (format!("{NEW_ROOT}/run"), 0o755),
            (format!("{NEW_ROOT}/home"), 0o755),
        ] {
            create_dir(&path, mode)?;
        }
    }

    let dev = format!("{NEW_ROOT}/dev");
    mount_tmpfs(&dev, sys::MS_NOSUID | sys::MS_NOEXEC, "mode=0755")?;
    create_dir(&format!("{dev}/pts"), 0o755)?;
    create_dir(&format!("{dev}/shm"), 0o1777)?;
    for (name, _, _) in DEVICE_NODES {
        let target = format!("{dev}/{name}");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        mount_bind_read_only(&format!("/dev/{name}"), &target)?;
    }

    mount_tmpfs(
        &format!("{dev}/shm"),
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        "mode=1777,size=536870912",
    )?;
    let devpts = cstring("devpts")?;
    let pts = cstring(&format!("{dev}/pts"))?;
    let pts_data = cstring(&format!(
        "newinstance,ptmxmode=0666,mode=0620,gid={}",
        identity.gid
    ))?;
    sys::mount(
        Some(&devpts),
        &pts,
        Some(&devpts),
        sys::MS_NOSUID | sys::MS_NOEXEC,
        Some(&pts_data),
    )
    .map_err(|e| io::Error::other(format!("mount fresh devpts: {e}")))?;

    symlink("pts/ptmx", format!("{dev}/ptmx"))?;
    symlink("/proc/self/fd", format!("{dev}/fd"))?;
    symlink("/proc/self/fd/0", format!("{dev}/stdin"))?;
    symlink("/proc/self/fd/1", format!("{dev}/stdout"))?;
    symlink("/proc/self/fd/2", format!("{dev}/stderr"))?;

    mount_tmpfs(
        &format!("{NEW_ROOT}/tmp"),
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        "mode=1777,size=268435456",
    )?;
    mount_tmpfs(
        &format!("{NEW_ROOT}/var/tmp"),
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        "mode=1777,size=268435456",
    )?;
    if let Some(application) = application {
        let run = format!("{NEW_ROOT}/run");
        mount_tmpfs(
            &run,
            sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
            "mode=0755,size=67108864",
        )?;
        create_dir(&format!("{run}/user"), 0o755)?;
        let runtime = format!("{run}/user/{}", identity.uid);
        create_dir(&runtime, 0o700)?;
        let application_runtime = format!("{runtime}/{}", crate::authority::RUNTIME_ROOT_NAME);
        create_dir(&application_runtime, 0o700)?;
        let wayland = format!("{runtime}/wayland-0");
        let listener = UnixListener::bind(&wayland)
            .map_err(|e| io::Error::other(format!("create Wayland bind target: {e}")))?;
        drop(listener);
        // A bind target must exist, and the kernel's graft check only refuses
        // mounting a directory over a non-directory and the reverse — a regular
        // file would do. A socket is used because it says what the path IS to
        // anyone reading the jail's runtime directory, and because the roster
        // check below is an exact set that a stray empty file would satisfy
        // just as well while meaning nothing. Binding a listener and dropping
        // it is simply how one makes a socket inode; it never accepts, and what
        // survives the drop is the inode.
        let bus = format!("{runtime}/bus");
        let listener = UnixListener::bind(&bus)
            .map_err(|e| io::Error::other(format!("create session bus bind target: {e}")))?;
        drop(listener);

        create_dir(&format!("{NEW_ROOT}/home/td"), 0o700)?;
        mount_application_tree(&application.package_files, &format!("{NEW_ROOT}/app"))?;
        mount_application_tree(&application.runtime_files, &format!("{NEW_ROOT}/usr"))?;
        mount_private_bind(
            &application.state.home,
            &format!("{NEW_ROOT}/home/td"),
            false,
        )?;
        for (source, target) in [
            (&application.state.config, ".config"),
            (&application.state.cache, ".cache"),
            (&application.state.data, ".local/share"),
            (&application.state.local_state, ".local/state"),
        ] {
            mount_private_bind(source, &format!("{NEW_ROOT}/home/td/{target}"), false)?;
        }
        mount_private_bind(&application.state.runtime, &application_runtime, false)?;
        mount_private_bind(&application.wayland_socket, &wayland, true)?;
        // Read-only like the Wayland socket. It does not cost the app anything:
        // `unix_find_other` asks for `MAY_WRITE` on the path, but `sb_permission`
        // returns `EROFS` only for regular files, directories and symlinks on a
        // read-only SUPERBLOCK, and `MNT_READONLY` is a vfsmount flag that
        // `inode_permission` never consults — it is enforced by
        // `mnt_want_write()` on the write paths. So `connect(2)` works, and
        // `SCM_RIGHTS` is socket-layer and untouched by mount flags.
        //
        // What it BUYS is `chmod`/`chown`, which do call `mnt_want_write()`. The
        // app owns this inode — uid 1000, mode 0600, and the jail maps
        // `1000 1000 1` — so without `MS_RDONLY` it could `chmod 0000` the
        // socket through its own bind and change the HOST's real bus socket,
        // denying `connect(2)` to the compositor, the portal and
        // `/etc/bootsuccess`'s probe. A draft of this comment said instead that
        // read-only stops the app replacing the socket: it does not, and nothing
        // here needs it to. Unlink is governed by the parent directory — the
        // jail's own rw tmpfs — and is refused because the path is a mountpoint
        // (`is_local_mountpoint` -> `EBUSY`) and the app has no `CAP_SYS_ADMIN`
        // to unmount it.
        mount_private_bind(&application.bus_socket, &bus, true)?;
        for filesystem in &application.filesystems {
            mount_filesystem_grant(filesystem)?;
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        for (source, target) in [
            (
                application.package_files.as_path(),
                format!("{NEW_ROOT}/app"),
            ),
            (
                application.runtime_files.as_path(),
                format!("{NEW_ROOT}/usr"),
            ),
            (
                application.state.home.as_path(),
                format!("{NEW_ROOT}/home/td"),
            ),
            (
                application.state.config.as_path(),
                format!("{NEW_ROOT}/home/td/.config"),
            ),
            (
                application.state.cache.as_path(),
                format!("{NEW_ROOT}/home/td/.cache"),
            ),
            (
                application.state.data.as_path(),
                format!("{NEW_ROOT}/home/td/.local/share"),
            ),
            (
                application.state.local_state.as_path(),
                format!("{NEW_ROOT}/home/td/.local/state"),
            ),
            (application.state.runtime.as_path(), application_runtime),
            (application.wayland_socket.as_path(), wayland),
            (application.bus_socket.as_path(), bus),
        ] {
            require_bind_source(&mountinfo, source, Path::new(&target))?;
        }
    }
    if application.is_none() {
        mount_reaper_probe(executable)?;
    }
    remount_read_only(&dev, sys::MS_NOSUID | sys::MS_NOEXEC)
}

fn mount_reaper_probe(executable: &Path) -> io::Result<()> {
    let target = format!("{NEW_ROOT}{REAPER_PROBE_PATH}");
    if fs::symlink_metadata(&target).is_ok() {
        return Err(io::Error::other("fresh reaper-probe path already exists"));
    }
    drop(OpenOptions::new().write(true).create_new(true).open(&target)?);
    mount_bind(executable, &target)?;
    remount_read_only(
        &target,
        sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV,
    )?;
    let metadata = fs::metadata(&target)?;
    if !metadata.file_type().is_file() || metadata.mode() & 0o111 == 0 {
        return Err(io::Error::other(
            "reaper-probe mount is not an executable regular file",
        ));
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_bind_source(&mountinfo, executable, Path::new(&target))
}

fn last_capability() -> io::Result<u32> {
    let last = fs::read_to_string("/proc/sys/kernel/cap_last_cap")?
        .trim()
        .parse::<u32>()
        .map_err(|e| io::Error::other(format!("invalid kernel cap_last_cap: {e}")))?;
    if last > MAX_CAPABILITY {
        return Err(io::Error::other(format!(
            "kernel capability {last} exceeds td-jail's 64-bit capability ABI"
        )));
    }
    Ok(last)
}

fn require_ambient(last: u32, expected: u64) -> io::Result<()> {
    for capability in 0..=last {
        let mask = 1_u64 << capability;
        if sys::ambient_capability(capability)? != (expected & mask != 0) {
            return Err(io::Error::other(format!(
                "ambient capability {capability} did not match the compiled set"
            )));
        }
    }
    Ok(())
}

fn require_empty_bounding(last: u32) -> io::Result<()> {
    for capability in 0..=last {
        if sys::bounding_capability(capability)? {
            return Err(io::Error::other(format!(
                "bounding capability {capability} survived the stage-1 drop"
            )));
        }
    }
    Ok(())
}

fn require_capability_rows(
    status: &str,
    sets: sys::CapabilitySets,
    ambient: u64,
    bounding: u64,
) -> io::Result<()> {
    for (key, expected) in [
        ("CapEff:", sets.effective),
        ("CapPrm:", sets.permitted),
        ("CapInh:", sets.inheritable),
        ("CapAmb:", ambient),
        ("CapBnd:", bounding),
    ] {
        if capability_row(status, key)? != expected {
            return Err(io::Error::other(format!(
                "/proc capability row {key} did not match its syscall readback"
            )));
        }
    }
    Ok(())
}

fn prepare_capability_bridge() -> io::Result<()> {
    let last = last_capability()?;
    let current = sys::capabilities()?;
    let required = SYS_ADMIN_MASK | SETPCAP_MASK;
    if current.effective & required != required || current.permitted & required != required {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "new user namespace did not grant CAP_SYS_ADMIN and CAP_SETPCAP",
        ));
    }

    sys::clear_ambient_capabilities()?;
    require_ambient(last, 0)?;
    let bridge = sys::CapabilitySets {
        effective: current.effective,
        permitted: current.permitted,
        inheritable: SYS_ADMIN_MASK,
    };
    sys::set_capabilities(bridge)?;
    if sys::capabilities()? != bridge {
        return Err(io::Error::other(
            "capset did not install the exact stage-1 inheritable bridge",
        ));
    }
    sys::raise_ambient_sys_admin()?;
    require_ambient(last, SYS_ADMIN_MASK)?;

    for capability in 0..=last {
        sys::drop_bounding_capability(capability)?;
        if sys::bounding_capability(capability)? {
            return Err(io::Error::other(format!(
                "bounding capability {capability} survived its drop"
            )));
        }
    }
    require_empty_bounding(last)?;
    let status = fs::read_to_string("/proc/self/status")?;
    require_capability_rows(&status, bridge, SYS_ADMIN_MASK, 0)
}

fn require_stage2_capabilities() -> io::Result<()> {
    let expected = sys::CapabilitySets {
        effective: SYS_ADMIN_MASK,
        permitted: SYS_ADMIN_MASK,
        inheritable: SYS_ADMIN_MASK,
    };
    let actual = sys::capabilities()?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("stage 2 capability sets are {actual:?}, expected {expected:?}"),
        ));
    }
    let last = last_capability()?;
    require_ambient(last, SYS_ADMIN_MASK)?;
    require_empty_bounding(last)?;
    let status = fs::read_to_string("/proc/self/status")?;
    require_capability_rows(&status, expected, SYS_ADMIN_MASK, 0)
}

fn clear_and_require_empty_capabilities() -> io::Result<()> {
    let last = last_capability()?;
    sys::clear_ambient_capabilities()?;
    require_ambient(last, 0)?;
    let empty = sys::CapabilitySets {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    };
    sys::set_capabilities(empty)?;
    if sys::capabilities()? != empty {
        return Err(io::Error::other(
            "stage 2 retained a capability after the final capset",
        ));
    }
    require_ambient(last, 0)?;
    require_empty_bounding(last)?;
    let status = fs::read_to_string("/proc/self/status")?;
    require_capability_rows(&status, empty, 0, 0)
}

fn require_runtime_confinement() -> io::Result<()> {
    if !sys::no_new_privileges()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "PR_GET_NO_NEW_PRIVS did not read back the installed restriction",
        ));
    }
    let status = fs::read_to_string("/proc/self/status")?;
    if decimal_status_row(&status, "NoNewPrivs:")? != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "/proc/self/status did not read back NoNewPrivs: 1",
        ));
    }
    if decimal_status_row(&status, "Seccomp:")? != 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "/proc/self/status did not read back seccomp filter mode",
        ));
    }
    Ok(())
}

fn install_standard_seccomp_filter() -> io::Result<()> {
    let program = seccomp::standard_program()?;
    sys::set_no_new_privileges()?;
    if !sys::no_new_privileges()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "PR_SET_NO_NEW_PRIVS succeeded without changing its readback",
        ));
    }
    sys::install_seccomp_filter(program.instructions())?;
    require_runtime_confinement()
}

fn enter_mount_plan() -> io::Result<()> {
    let procfs = cstring("proc")?;
    let proc_target = cstring(&format!("{NEW_ROOT}/proc"))?;
    sys::mount(
        Some(&procfs),
        &proc_target,
        Some(&procfs),
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        None,
    )
    .map_err(|e| io::Error::other(format!("mount fresh PID-namespace procfs: {e}")))?;

    let new_root = cstring(NEW_ROOT)?;
    let put_old = cstring(PUT_OLD)?;
    sys::pivot_root(&new_root, &put_old)
        .map_err(|e| io::Error::other(format!("pivot into fresh root: {e}")))?;
    std::env::set_current_dir("/")?;
    let old_root = cstring(OLD_ROOT)?;
    sys::umount_detach(&old_root).map_err(|e| io::Error::other(format!("detach old root: {e}")))?;
    fs::remove_dir(OLD_ROOT)?;
    remount_read_only("/", sys::MS_NOSUID | sys::MS_NODEV)
}

fn read_dir_names(path: &str) -> io::Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(path)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other(format!("{path} contains a non-UTF-8 entry name")))?;
        names.insert(name);
    }
    Ok(names)
}

fn require_names(path: &str, expected: &[&str]) -> io::Result<()> {
    let expected = expected
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    let actual = read_dir_names(path)?;
    if actual != expected {
        return Err(io::Error::other(format!(
            "{path} entries are {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn require_mode(path: &str, expected: u32) -> io::Result<()> {
    let actual = fs::metadata(path)?.mode() & 0o7777;
    if actual != expected {
        return Err(io::Error::other(format!(
            "{path} mode is {actual:#o}, expected {expected:#o}"
        )));
    }
    Ok(())
}

fn device_numbers(raw: u64) -> (u64, u64) {
    let major = ((raw >> 8) & 0xfff) | ((raw >> 32) & 0xffff_f000);
    let minor = (raw & 0xff) | ((raw >> 12) & 0xffff_ff00);
    (major, minor)
}

fn require_devices() -> io::Result<()> {
    require_names(
        "/dev",
        &[
            "fd", "full", "null", "ptmx", "pts", "random", "shm", "stderr", "stdin", "stdout",
            "urandom", "zero",
        ],
    )?;
    for (name, expected_major, expected_minor) in DEVICE_NODES {
        let path = format!("/dev/{name}");
        let metadata = fs::metadata(&path)?;
        if !metadata.file_type().is_char_device() {
            return Err(io::Error::other(format!(
                "{path} is not a character device"
            )));
        }
        let (major, minor) = device_numbers(metadata.rdev());
        if (major, minor) != (*expected_major, *expected_minor) {
            return Err(io::Error::other(format!(
                "{path} is device {major}:{minor}, expected {expected_major}:{expected_minor}"
            )));
        }
    }
    for (path, target) in [
        ("/dev/ptmx", "pts/ptmx"),
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        if fs::read_link(path)? != *target {
            return Err(io::Error::other(format!(
                "{path} does not target the compiled descriptor path"
            )));
        }
    }
    OpenOptions::new()
        .write(true)
        .open("/dev/null")?
        .write_all(b"td-jail-device-probe")?;

    let ptmx = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/ptmx")?;
    let slave = fs::metadata("/dev/pts/0")?;
    let identity = current_identity()?;
    if !slave.file_type().is_char_device()
        || slave.uid() != identity.uid
        || slave.gid() != identity.gid
        || slave.mode() & 0o7777 != 0o620
    {
        return Err(io::Error::other(
            "fresh devpts did not create the compiled 0620 application-owned slave",
        ));
    }
    drop(ptmx);
    Ok(())
}

fn require_mount(
    mountinfo: &str,
    target: &str,
    filesystem: Option<&str>,
    required: &[&str],
    forbidden: &[&str],
) -> io::Result<()> {
    let mut found = None;
    for line in mountinfo.lines() {
        let (left, right) = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?;
        let mut fields = left.split_whitespace();
        let mountpoint = fields
            .nth(4)
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        let options = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no mount options"))?;
        if mountpoint != target {
            continue;
        }
        if found.is_some() {
            return Err(io::Error::other(format!(
                "mountinfo contains duplicate {target} mounts"
            )));
        }
        let actual_filesystem = right
            .split_whitespace()
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no filesystem"))?;
        let options = options.split(',').collect::<BTreeSet<_>>();
        found = Some((actual_filesystem, options));
    }
    let (actual_filesystem, options) =
        found.ok_or_else(|| io::Error::other(format!("mountinfo contains no {target} mount")))?;
    if let Some(expected) = filesystem {
        if actual_filesystem != expected {
            return Err(io::Error::other(format!(
                "{target} uses {actual_filesystem}, expected {expected}"
            )));
        }
    }
    for option in required {
        if !options.contains(option) {
            return Err(io::Error::other(format!(
                "{target} mount lacks required option {option}"
            )));
        }
    }
    for option in forbidden {
        if options.contains(option) {
            return Err(io::Error::other(format!(
                "{target} mount carries forbidden option {option}"
            )));
        }
    }
    Ok(())
}

fn require_mount_super_option(mountinfo: &str, target: &str, required: &str) -> io::Result<()> {
    for line in mountinfo.lines() {
        let (left, right) = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?;
        let mountpoint = left
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        if mountpoint != target {
            continue;
        }
        let super_options = right
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| io::Error::other("mountinfo row has no superblock options"))?;
        if super_options.split(',').any(|option| option == required) {
            return Ok(());
        }
        return Err(io::Error::other(format!(
            "{target} mount lacks required superblock option {required}"
        )));
    }
    Err(io::Error::other(format!(
        "mountinfo contains no {target} mount"
    )))
}

fn require_bind_source(mountinfo: &str, source: &Path, target: &Path) -> io::Result<()> {
    let source_identity = mount_identity_for_path(mountinfo, source)?;
    let target_identity = mount_identity_for_path(mountinfo, target)?;
    if source_identity != target_identity {
        return Err(io::Error::other(format!(
            "{} binds {target_identity:?}, expected source {} as {source_identity:?}",
            target.display(),
            source.display()
        )));
    }
    Ok(())
}

fn writable_probe_path(path: &str, token: &[u8; TOKEN_LEN]) -> PathBuf {
    Path::new(path).join(format!("{WRITE_PROBE_PREFIX}{}", encode_token(token)))
}

fn require_writable_directory(path: &str, token: &[u8; TOKEN_LEN]) -> io::Result<()> {
    let probe = writable_probe_path(path, token);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("create writable probe {}: {error}", probe.display()),
            )
        })?;
    match fs::remove_file(&probe) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("unlink writable probe {}: {error}", probe.display()),
            ));
        }
    }
    file.write_all(b"ok").map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("write writable probe {}: {error}", probe.display()),
        )
    })
}

fn require_read_only_mount(path: &str, token: &[u8; TOKEN_LEN]) -> io::Result<()> {
    let probe = writable_probe_path(path, token);
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Err(error) if error.kind() == io::ErrorKind::ReadOnlyFilesystem => Ok(()),
        Err(error) => Err(io::Error::other(format!(
            "immutable jail tree refused {} for {error}, not read-only-filesystem",
            probe.display()
        ))),
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            Err(io::Error::other(format!(
                "immutable jail tree accepted a write at {}",
                probe.display()
            )))
        }
    }
}

fn grant_scaffold_names(
    application: bool,
    filesystems: &[Stage2Filesystem],
) -> io::Result<BTreeMap<PathBuf, BTreeSet<String>>> {
    let root = if application {
        ["app", "dev", "home", "proc", "run", "tmp", "usr", "var"].as_slice()
    } else {
        ["dev", "proc", "tmp", "var"].as_slice()
    };
    let mut expected = BTreeMap::from([
        (
            PathBuf::from("/"),
            root.iter().map(|name| (*name).to_string()).collect(),
        ),
        (PathBuf::from("/var"), BTreeSet::from(["tmp".to_string()])),
    ]);
    if application {
        expected.insert(
            PathBuf::from("/home"),
            BTreeSet::from(["td".to_string()]),
        );
    }
    for filesystem in filesystems {
        let mut parent = PathBuf::from("/");
        for component in filesystem.target.components() {
            let name = component.as_os_str();
            if name == "/" {
                continue;
            }
            if path_is_same_or_child(&parent, Path::new("/home/td")) {
                break;
            }
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("filesystem target component is not UTF-8"))?;
            expected
                .entry(parent.clone())
                .or_default()
                .insert(name.to_string());
            parent.push(name);
            if parent == filesystem.target {
                break;
            }
            if parent == Path::new("/home/td") {
                break;
            }
            expected.entry(parent.clone()).or_default();
        }
    }
    Ok(expected)
}

fn require_mount_plan(
    filesystems: Option<&[Stage2Filesystem]>,
    token: &[u8; TOKEN_LEN],
    identity: Identity,
) -> io::Result<()> {
    let application = filesystems.is_some();
    if fs::symlink_metadata(OLD_ROOT).is_ok() || fs::symlink_metadata("/etc").is_ok() {
        return Err(io::Error::other(
            "detached host root remains reachable in the fresh root",
        ));
    }
    for (path, expected) in grant_scaffold_names(application, filesystems.unwrap_or_default())? {
        let path_text = path
            .to_str()
            .ok_or_else(|| io::Error::other("filesystem scaffold path is not UTF-8"))?;
        if read_dir_names(path_text)? != expected {
            return Err(io::Error::other(format!(
                "fresh scaffold {} entries do not match the mount plan",
                path.display()
            )));
        }
    }
    require_mode("/", 0o755)?;
    require_mode("/dev", 0o755)?;
    require_mode("/dev/shm", 0o1777)?;
    require_mode("/tmp", 0o1777)?;
    require_mode("/var/tmp", 0o1777)?;
    require_devices()?;

    let numeric = read_dir_names("/proc")?
        .into_iter()
        .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        .collect::<BTreeSet<_>>();
    if numeric != BTreeSet::from(["1".to_string()]) {
        return Err(io::Error::other(format!(
            "fresh procfs exposes PIDs {numeric:?}, expected only PID 1"
        )));
    }
    if fs::read_link("/proc/self")?.as_os_str() != "1" {
        return Err(io::Error::other(
            "fresh procfs does not resolve self to PID 1",
        ));
    }

    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_mount(
        &mountinfo,
        "/",
        Some("tmpfs"),
        &["ro", "nosuid", "nodev"],
        &["rw"],
    )?;
    require_mount(
        &mountinfo,
        "/proc",
        Some("proc"),
        &["rw", "nosuid", "nodev", "noexec"],
        &["ro"],
    )?;
    require_mount(
        &mountinfo,
        "/dev",
        Some("tmpfs"),
        &["ro", "nosuid", "noexec"],
        &["rw", "nodev"],
    )?;
    for path in ["/tmp", "/var/tmp"] {
        require_mount(
            &mountinfo,
            path,
            Some("tmpfs"),
            &["rw", "nosuid", "nodev", "noexec"],
            &["ro"],
        )?;
        require_mount_super_option(&mountinfo, path, "size=262144k")?;
        require_writable_directory(path, token)?;
    }
    require_mount(
        &mountinfo,
        "/dev/shm",
        Some("tmpfs"),
        &["rw", "nosuid", "nodev", "noexec"],
        &["ro"],
    )?;
    require_mount_super_option(&mountinfo, "/dev/shm", "size=524288k")?;
    require_mount(
        &mountinfo,
        "/dev/pts",
        Some("devpts"),
        &["rw", "nosuid", "noexec"],
        &["ro", "nodev"],
    )?;
    for (name, _, _) in DEVICE_NODES {
        require_mount(
            &mountinfo,
            &format!("/dev/{name}"),
            None,
            &["ro", "nosuid", "noexec"],
            &["rw", "nodev"],
        )?;
    }

    if application {
        let filesystems = filesystems.unwrap_or_default();
        require_names("/run", &["user"])?;
        let runtime = format!("/run/user/{}", identity.uid);
        require_mode(&runtime, 0o700)?;
        require_names("/run/user", &[&identity.uid.to_string()])?;
        require_names(
            &runtime,
            &[crate::authority::RUNTIME_ROOT_NAME, "bus", "wayland-0"],
        )?;
        for path in ["/app", "/usr"] {
            require_mount(
                &mountinfo,
                path,
                None,
                &["ro", "nosuid", "nodev"],
                &["rw", "noexec"],
            )?;
        }
        require_mount(
            &mountinfo,
            "/run",
            Some("tmpfs"),
            &["rw", "nosuid", "nodev", "noexec"],
            &["ro"],
        )?;
        require_mount(
            &mountinfo,
            "/home/td",
            None,
            &["rw", "nosuid", "nodev", "noexec"],
            &["ro"],
        )?;
        for path in [
            "/home/td/.config",
            "/home/td/.cache",
            "/home/td/.local/share",
            "/home/td/.local/state",
        ] {
            require_mount(
                &mountinfo,
                path,
                None,
                &["rw", "nosuid", "nodev", "noexec"],
                &["ro"],
            )?;
            require_writable_directory(path, token)?;
        }
        require_mount(
            &mountinfo,
            &format!("{runtime}/wayland-0"),
            None,
            &["ro", "nosuid", "nodev", "noexec"],
            &["rw"],
        )?;
        require_mount(
            &mountinfo,
            &format!("{runtime}/bus"),
            None,
            &["ro", "nosuid", "nodev", "noexec"],
            &["rw"],
        )?;
        let application_runtime = format!("{runtime}/{}", crate::authority::RUNTIME_ROOT_NAME);
        require_mode(&application_runtime, 0o700)?;
        require_mode("/home/td", 0o700)?;
        require_mount(
            &mountinfo,
            &application_runtime,
            None,
            &["rw", "nosuid", "nodev", "noexec"],
            &["ro"],
        )?;
        require_mount_super_option(&mountinfo, "/run", "size=65536k")?;
        require_writable_directory(&application_runtime, token)?;
        require_writable_directory("/home/td", token)?;
        for path in ["/app", "/usr"] {
            require_read_only_mount(path, token)?;
        }
        for filesystem in filesystems {
            require_grant_mount_policy(&mountinfo, filesystem)?;
            match (filesystem.source_kind, filesystem.read_only) {
                (FilesystemSourceKind::Directory, true) => {
                    require_read_only_mount(
                        filesystem
                            .target
                            .to_str()
                            .ok_or_else(|| io::Error::other("filesystem target is not UTF-8"))?,
                        token,
                    )?;
                }
                (FilesystemSourceKind::Directory, false) => {
                    require_writable_directory(
                        filesystem
                            .target
                            .to_str()
                            .ok_or_else(|| io::Error::other("filesystem target is not UTF-8"))?,
                        token,
                    )?;
                }
                (FilesystemSourceKind::File, true) => {}
                (FilesystemSourceKind::File, false) => {
                    require_writable_file(&filesystem.target)?;
                }
            }
        }
    } else {
        require_names("/tmp", &["td-jail-reaper-probe"])?;
        require_mount(
            &mountinfo,
            REAPER_PROBE_PATH,
            None,
            &["ro", "nosuid", "nodev"],
            &["rw", "noexec"],
        )?;
    }

    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("/root-write-probe")
        .is_ok()
    {
        return Err(io::Error::other("fresh root remained writable"));
    }
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("/dev/write-probe")
        .is_ok()
    {
        return Err(io::Error::other("fresh /dev remained mutable"));
    }
    require_stage2_capabilities()
}

fn require_empty_child_capabilities() -> io::Result<()> {
    let empty = sys::CapabilitySets {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    };
    let last = last_capability()?;
    if sys::capabilities()? != empty {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reaper probe inherited a live capability",
        ));
    }
    require_ambient(last, 0)?;
    require_empty_bounding(last)?;
    let status = fs::read_to_string("/proc/self/status")?;
    require_capability_rows(&status, empty, 0, 0)?;
    require_runtime_confinement()
}

pub fn run_reaper_child() -> io::Result<()> {
    run_descendant_parent(REAPER_ORPHAN_ARG)
}

pub fn run_survivor_child() -> io::Result<()> {
    run_descendant_parent(SURVIVOR_ORPHAN_ARG)
}

fn run_descendant_parent(orphan_argument: &str) -> io::Result<()> {
    if std::process::id() == 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reaper child unexpectedly became PID 1",
        ));
    }
    require_empty_child_capabilities()?;
    let child = Command::new(REAPER_PROBE_PATH)
        .arg(orphan_argument)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn zero-capability reaper orphan: {e}")))?;
    writeln!(io::stdout(), "{}", child.id())?;
    // Return without waiting so the grandchild reparents to PID 1.
    drop(child);
    Ok(())
}

pub fn run_reaper_orphan() -> io::Result<()> {
    run_descendant_orphan(Duration::from_millis(100))
}

pub fn run_survivor_orphan() -> io::Result<()> {
    run_descendant_orphan(SURVIVOR_PROBE_LIFETIME)
}

fn run_descendant_orphan(lifetime: Duration) -> io::Result<()> {
    if std::process::id() == 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reaper orphan unexpectedly became PID 1",
        ));
    }
    require_empty_child_capabilities()?;
    std::thread::sleep(lifetime);
    Ok(())
}

fn read_reported_pid(reader: impl Read, role: &str) -> io::Result<i32> {
    let mut report = String::new();
    reader.take(33).read_to_string(&mut report)?;
    let encoded = report
        .strip_suffix('\n')
        .ok_or_else(|| io::Error::other(format!("{role} report lacks its line terminator")))?;
    if encoded.is_empty() || !encoded.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::other(format!(
            "{role} returned an invalid PID: {report:?}"
        )));
    }
    encoded
        .parse::<i32>()
        .map_err(|e| io::Error::other(format!("{role} PID is invalid: {e}")))
}

fn probe_pid1_lifecycle() -> io::Result<()> {
    let mut child = Command::new(REAPER_PROBE_PATH)
        .arg(REAPER_CHILD_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn zero-capability reaper child: {e}")))?;
    let deadline = Instant::now() + REAPER_TIMEOUT;
    let direct_pid = i32::try_from(child.id())
        .map_err(|e| io::Error::other(format!("reaper child PID is invalid: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("reaper child stdout pipe was not created"))?;
    // The oracle must collect both descendants through wait4(-1).
    drop(child);

    let mut reaped = BTreeSet::new();
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "PID 1 did not empty the probe child table before the deadline: {reaped:?}"
            )));
        }
        match sys::wait_any(true)? {
            sys::Reaped::Child { pid, status } => {
                if status != 0 {
                    return Err(io::Error::other(format!(
                        "reaper probe child {pid} returned raw status {status:#x}"
                    )));
                }
                if !reaped.insert(pid) || reaped.len() > 2 {
                    return Err(io::Error::other(format!(
                        "PID 1 reaped an invalid initial child set {reaped:?}"
                    )));
                }
            }
            sys::Reaped::NotYet => std::thread::sleep(REAPER_POLL),
            sys::Reaped::NoChildren => break,
        }
    }

    // ECHILD guarantees no descendant can retain the report pipe writer.
    let orphan_pid = read_reported_pid(stdout, "reaper orphan")?;
    if direct_pid <= 1 || orphan_pid <= 1 || direct_pid == orphan_pid {
        return Err(io::Error::other(format!(
            "reaper probe PIDs are direct={direct_pid}, orphan={orphan_pid}"
        )));
    }
    let expected = BTreeSet::from([direct_pid, orphan_pid]);
    if reaped != expected {
        return Err(io::Error::other(format!(
            "PID 1 reaped {reaped:?}, expected {expected:?}"
        )));
    }
    probe_pid1_survivor_cleanup()
}

fn probe_pid1_survivor_cleanup() -> io::Result<()> {
    let term_pid = spawn_survivor_orphan("TERM survivor")?;
    let term_reaped = terminate_and_reap_survivors()?;
    require_single_survivor_signal(&term_reaped, term_pid, sys::SIGTERM, "TERM cleanup")?;

    let kill_pid = spawn_survivor_orphan("KILL survivor")?;
    let mut kill_reaped = SurvivorReap::default();
    let outcome = reap_survivors_until(
        Instant::now() + SURVIVOR_KILL_TIMEOUT,
        &mut kill_reaped,
        true,
    )?;
    if outcome != DrainOutcome::Drained {
        return Err(io::Error::other(format!(
            "survivor KILL probe missed ECHILD before its deadline: {kill_reaped:?}"
        )));
    }
    require_single_survivor_signal(&kill_reaped, kill_pid, sys::SIGKILL, "KILL cleanup")
}

fn spawn_survivor_orphan(role: &str) -> io::Result<i32> {
    let mut child = Command::new(REAPER_PROBE_PATH)
        .arg(SURVIVOR_CHILD_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn {role} parent: {e}")))?;
    let deadline = Instant::now() + REAPER_TIMEOUT;
    let direct_pid = i32::try_from(child.id())
        .map_err(|e| io::Error::other(format!("survivor child PID is invalid: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("survivor child stdout pipe was not created"))?;
    drop(child);

    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "PID 1 did not reap the {role} parent before the deadline"
            )));
        }
        match sys::wait_any(true)? {
            sys::Reaped::Child { pid, status } if pid == direct_pid && status == 0 => break,
            sys::Reaped::Child { pid, status } => {
                return Err(io::Error::other(format!(
                    "{role} probe reaped unexpected child {pid} with raw status {status:#x}"
                )));
            }
            sys::Reaped::NotYet => std::thread::sleep(REAPER_POLL),
            sys::Reaped::NoChildren => {
                return Err(io::Error::other(format!(
                    "{role} parent disappeared without a wait status"
                )));
            }
        }
    }

    // run_descendant_parent gives the orphan /dev/null, so the reaped parent
    // was the report pipe's only writer.
    let orphan_pid = read_reported_pid(stdout, role)?;
    if direct_pid <= 1 || orphan_pid <= 1 || direct_pid == orphan_pid {
        return Err(io::Error::other(format!(
            "{role} PIDs are direct={direct_pid}, orphan={orphan_pid}"
        )));
    }
    Ok(orphan_pid)
}

fn require_single_survivor_signal(
    reaped: &SurvivorReap,
    expected_pid: i32,
    expected_signal: i32,
    role: &str,
) -> io::Result<()> {
    let observed = reaped
        .sole_child
        .and_then(|(pid, status)| sys::wait_signal(status).map(|signal| (pid, signal)));
    if reaped.count != 1 || observed != Some((expected_pid, expected_signal)) {
        return Err(io::Error::other(format!(
            "{role} reaped {reaped:?}, expected PID {expected_pid} by signal {expected_signal}"
        )));
    }
    Ok(())
}

pub fn spawn_application_session<I>(argv0: OsString, arguments: I) -> io::Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg(APPLICATION_SESSION_ARG)
        .arg(std::process::id().to_string())
        .arg(argv0)
        .args(arguments);
    let status = command.status().map_err(|error| {
        io::Error::other(format!("spawn application containment bootstrap: {error}"))
    })?;
    if status.success() {
        Ok(())
    } else if let Some(code) = status.code() {
        std::process::exit(code)
    } else if let Some(signal) = status.signal() {
        std::process::exit(128_i32.saturating_add(signal))
    } else {
        Err(io::Error::other(format!(
            "application launch ended without an exit status: {status}"
        )))
    }
}

pub fn contain_application_session(expected_parent: u32) -> io::Result<()> {
    sys::set_parent_death_signal()?;
    let before = fs::read_to_string("/proc/self/stat")?;
    require_direct_parent(expected_parent, &before)?;
    let containment = process_containment(&before)?;
    if containment.terminal == 0 && containment.process_group == expected_parent {
        let after = fs::read_to_string("/proc/self/stat")?;
        return require_supervised_group("application launcher", expected_parent, &after);
    }
    let session = sys::start_new_session()?;
    let after = fs::read_to_string("/proc/self/stat")?;
    require_detached_session("application launcher", session, &after)
}

pub fn probe_transition() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail requires the nonzero application identity",
        ));
    }
    let before = NamespaceSnapshot::read()?;
    let token = random_token()?;
    let executable = std::env::current_exe()?;

    sys::unshare_namespaces(true)?;
    install_identity_maps(identity, identity)?;
    NamespaceSnapshot::read()?.require_all_changed(&before)?;
    sys::bring_up_loopback()
        .map_err(|e| io::Error::other(format!("bring up isolated loopback: {e}")))?;
    let test_leak = install_test_leak_if_requested()?;
    close_inherited_descriptors(None)?;
    if let Some(descriptor) = test_leak {
        require_descriptor_closed(descriptor)?;
    }
    prepare_mount_plan(identity, &executable, None)?;
    prepare_capability_bridge()?;

    let (proof_reader, mut proof_writer) = io::pipe()?;
    let (stage2_output, stage2_writer) = io::pipe()?;
    let stage2_error_writer = stage2_writer.try_clone()?;
    let mut child = Command::new(executable)
        .arg(STAGE2_ARG)
        .arg(encode_token(&token))
        .arg(identity.uid.to_string())
        .arg(identity.gid.to_string())
        .arg(identity.uid.to_string())
        .arg(identity.gid.to_string())
        .arg(STAGE2_PROBE_ARG)
        .stdin(Stdio::from(proof_reader))
        .stdout(Stdio::from(stage2_writer))
        .stderr(Stdio::from(stage2_error_writer))
        .spawn()?;

    if let Err(error) = require_child_pid_namespace_changed(&before, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    if let Err(error) = proof_writer.write_all(&token) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(proof_writer);

    let expected = format!("{STAGE2_MARKER} pid=1\n");
    let mut response = Vec::new();
    if let Err(error) = stage2_output
        .take((STAGE2_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut response)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if response.len() > STAGE2_OUTPUT_LIMIT {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("stage-2 output exceeded 4096 bytes"));
    }
    let status = child.wait()?;
    let response = String::from_utf8(response)
        .map_err(|e| io::Error::other(format!("stage-2 output is not UTF-8: {e}")))?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "stage 2 refused the namespace transition: {}: {}",
            status,
            response.trim()
        )));
    }
    if response != expected {
        return Err(io::Error::other(format!(
            "stage 2 returned an unexpected transition response: {response:?}"
        )));
    }
    writeln!(io::stdout(), "{TRANSITION_MARKER} pid=1")
}

pub fn probe_resource_caps(application: &str) -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail requires the nonzero application identity",
        ));
    }
    let limits = authority::resolve_resource_limits(application)?;
    let diagnostic = cgroup::probe_active(application, limits, identity.uid, identity.gid)?;
    writeln!(io::stdout(), "{diagnostic}")
}

pub fn run_cgroup_cleanup_bootstrap(membership: &str) -> io::Result<()> {
    let _identity = cleanup_identity()?;
    cgroup::validate_expected_membership(membership)?;
    let session = sys::start_new_session()?;
    require_detached_session(
        "cgroup cleanup bootstrap",
        session,
        &fs::read_to_string("/proc/self/stat")?,
    )?;

    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg(CGROUP_CLEANUP_WATCH_ARG)
        .arg(membership)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut watcher = command
        .spawn()
        .map_err(|error| io::Error::other(format!("spawn cgroup cleanup watcher: {error}")))?;
    drop(command);
    let status = loop {
        match watcher.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => break result?,
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cgroup cleanup watcher exited unsuccessfully: {status}"
        )))
    }
}

pub fn run_cgroup_cleanup_watcher(membership: &str) -> io::Result<()> {
    let identity = cleanup_identity()?;
    cgroup::validate_expected_membership(membership)?;
    let session = sys::start_new_session()?;
    require_detached_session(
        "cgroup cleanup watcher",
        session,
        &fs::read_to_string("/proc/self/stat")?,
    )?;
    {
        let mut readiness = io::stdout().lock();
        readiness.write_all(&CGROUP_CLEANUP_READY)?;
        readiness.flush()?;
    }
    wait_for_parent_end(&mut io::stdin())?;
    cgroup::remove_abandoned(membership, identity.uid, identity.gid)
}

fn cleanup_identity() -> io::Result<Identity> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail cgroup cleanup requires the nonzero application identity",
        ));
    }
    Ok(identity)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessContainment {
    parent: u32,
    process_group: u32,
    session: u32,
    terminal: i64,
}

fn process_containment(stat: &str) -> io::Result<ProcessContainment> {
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("/proc/self/stat has no command-field terminator"))?;
    let mut fields = fields.split_whitespace();
    let _state = fields
        .next()
        .ok_or_else(|| io::Error::other("/proc/self/stat has no state field"))?;
    let parent = parse_proc_stat_u32(&mut fields, "parent")?;
    let process_group = parse_proc_stat_u32(&mut fields, "process group")?;
    let session = parse_proc_stat_u32(&mut fields, "session")?;
    let terminal = fields
        .next()
        .ok_or_else(|| io::Error::other("/proc/self/stat has no controlling-terminal field"))?
        .parse::<i64>()
        .map_err(|error| {
            io::Error::other(format!(
                "invalid /proc/self/stat controlling-terminal field: {error}"
            ))
        })?;
    Ok(ProcessContainment {
        parent,
        process_group,
        session,
        terminal,
    })
}

fn require_direct_parent(expected: u32, stat: &str) -> io::Result<()> {
    let observed = process_containment(stat)?.parent;
    if observed != expected {
        return Err(io::Error::other(format!(
            "application launch parent changed from {expected} to {observed} before containment"
        )));
    }
    Ok(())
}

fn require_detached_session(role: &str, expected: u32, stat: &str) -> io::Result<()> {
    let containment = process_containment(stat)?;
    if containment.process_group != expected
        || containment.session != expected
        || containment.terminal != 0
    {
        return Err(io::Error::other(format!(
            "{role} containment read back as process-group={}, session={}, \
             controlling-terminal={}; expected detached session {expected}",
            containment.process_group, containment.session, containment.terminal
        )));
    }
    Ok(())
}

fn require_supervised_group(role: &str, expected: u32, stat: &str) -> io::Result<()> {
    let containment = process_containment(stat)?;
    if containment.process_group != expected || containment.terminal != 0 {
        return Err(io::Error::other(format!(
            "{role} containment read back as process-group={}, controlling-terminal={}; \
             expected no-terminal supervisor group {expected}",
            containment.process_group, containment.terminal
        )));
    }
    Ok(())
}

fn parse_proc_stat_u32<'a>(
    fields: &mut impl Iterator<Item = &'a str>,
    name: &str,
) -> io::Result<u32> {
    fields
        .next()
        .ok_or_else(|| io::Error::other(format!("/proc/self/stat has no {name} field")))?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid /proc/self/stat {name}: {error}")))
}

fn wait_for_parent_end(reader: &mut impl Read) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cgroup cleanup keepalive carried unexpected data",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

pub fn run_stage2(
    expected: [u8; TOKEN_LEN],
    expected_identity: Identity,
    expected_outside_identity: Identity,
    action: Stage2Action,
) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "internal stage 2 is not PID 1 of a new namespace",
        ));
    }

    let mut actual = [0_u8; TOKEN_LEN];
    let mut stdin = io::stdin().lock();
    stdin.read_exact(&mut actual)?;
    if !tokens_equal(&actual, &expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "internal stage-2 proof does not match",
        ));
    }
    drop(stdin);
    sys::set_parent_death_signal()?;
    require_only_stdio_descriptors()?;

    let status = fs::read_to_string("/proc/self/status")?;
    let identity = Identity {
        uid: effective_id(&status, "Uid:")?,
        gid: effective_id(&status, "Gid:")?,
    };
    if identity != expected_identity {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage-2 credentials do not match stage 1",
        ));
    }
    require_single_map(
        "/proc/self/uid_map",
        identity.uid,
        Some(expected_outside_identity.uid),
    )?;
    require_single_map(
        "/proc/self/gid_map",
        identity.gid,
        Some(expected_outside_identity.gid),
    )?;
    require_stage2_capabilities()?;
    enter_mount_plan()?;
    let mount_probe_token = random_token()?;
    let filesystems = match &action {
        Stage2Action::Probe => None,
        Stage2Action::Launch { filesystems, .. } => Some(filesystems.as_slice()),
    };
    require_mount_plan(filesystems, &mount_probe_token, identity)?;
    clear_and_require_empty_capabilities()?;
    let host_mode = matches!(
        &action,
        Stage2Action::Launch {
            cgroup_membership,
            ..
        } if cgroup_membership == NO_CGROUP_MEMBERSHIP
    );
    install_standard_seccomp_filter().map_err(|error| {
        if host_mode {
            io::Error::new(
                error.kind(),
                format!("host mode requires the standard seccomp filter: {error}"),
            )
        } else {
            error
        }
    })?;
    match action {
        Stage2Action::Probe => {
            probe_pid1_lifecycle()?;
            writeln!(io::stdout(), "{STAGE2_MARKER} pid=1")
        }
        Stage2Action::Launch {
            entry,
            environment,
            filesystems: _,
            resources,
            cgroup_membership,
            arguments,
        } => {
            if cgroup_membership != NO_CGROUP_MEMBERSHIP {
                cgroup::require_current_membership(&cgroup_membership)?;
            }
            sys::set_and_require_data_limit(resources.memory_max_bytes)?;
            sys::set_dumpable(false)?;
            if sys::dumpable()? {
                return Err(io::Error::other("PID 1 remained dumpable"));
            }
            start_stage1_liveness_watcher()?;
            run_application(&entry, &environment, &arguments)
        }
    }
}

fn start_stage1_liveness_watcher() -> io::Result<()> {
    let watcher = std::thread::Builder::new()
        .name("td-jail-stage1-liveness".to_string())
        .spawn(|| {
            wait_for_stage1_end(&mut io::stdin());
            std::process::exit(125);
        })?;
    drop(watcher);
    Ok(())
}

fn wait_for_stage1_end(reader: &mut impl Read) {
    let mut unexpected = [0_u8; 1];
    loop {
        match reader.read(&mut unexpected) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => return,
        }
    }
}

fn run_application(
    entry: &str,
    environment: &[(OsString, OsString)],
    arguments: &[OsString],
) -> io::Result<()> {
    let null_input = fs::File::open("/dev/null")?;
    let null_output = OpenOptions::new().write(true).open("/dev/null")?;
    let null_error = OpenOptions::new().write(true).open("/dev/null")?;
    let mut command = Command::new(entry);
    command
        .args(arguments)
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::from(null_input))
        .stdout(Stdio::from(null_output))
        .stderr(Stdio::from(null_error));
    let child = command.spawn();
    drop(command);
    let child = child
        .map_err(|e| io::Error::other(format!("launch application entry {entry}: {e}")))?;
    let application_pid = i32::try_from(child.id())
        .map_err(|e| io::Error::other(format!("application PID is invalid: {e}")))?;
    drop(child);

    let mut application_status = None;
    loop {
        match sys::wait_any(false)? {
            sys::Reaped::Child { pid, status } => {
                if pid == application_pid {
                    application_status = Some(status);
                    break;
                }
            }
            sys::Reaped::NotYet => continue,
            sys::Reaped::NoChildren => break,
        }
    }
    let cleanup_error = match application_status {
        Some(_) => terminate_and_reap_survivors().err(),
        None => None,
    };
    match (application_status, cleanup_error) {
        (Some(0), None) => Ok(()),
        (Some(status), None) => Err(io::Error::other(format!(
            "application entry returned raw status {status:#x}"
        ))),
        (Some(status), Some(error)) => Err(io::Error::other(format!(
            "application entry returned raw status {status:#x}; survivor cleanup failed: {error}"
        ))),
        (None, _) => Err(io::Error::other(
            "application entry disappeared without a wait status",
        )),
    }
}

fn terminate_and_reap_survivors() -> io::Result<SurvivorReap> {
    terminate_and_reap_survivors_with(sys::terminate_namespace, |timeout, reaped, force_kill| {
        reap_survivors_until(Instant::now() + timeout, reaped, force_kill)
    })
}

fn terminate_and_reap_survivors_with<T, D>(
    mut terminate: T,
    mut drain: D,
) -> io::Result<SurvivorReap>
where
    T: FnMut() -> io::Result<()>,
    D: FnMut(Duration, &mut SurvivorReap, bool) -> io::Result<DrainOutcome>,
{
    let mut reaped = SurvivorReap::default();
    terminate()?;
    if drain(SURVIVOR_TERM_TIMEOUT, &mut reaped, false)? == DrainOutcome::Drained {
        return Ok(reaped);
    }

    if drain(SURVIVOR_KILL_TIMEOUT, &mut reaped, true)? == DrainOutcome::Drained {
        return Ok(reaped);
    }
    Err(io::Error::other(format!(
        "survivor cleanup reached the SIGKILL deadline before PID 1 observed ECHILD: {reaped:?}"
    )))
}

fn reap_survivors_until(
    deadline: Instant,
    reaped: &mut SurvivorReap,
    force_kill: bool,
) -> io::Result<DrainOutcome> {
    loop {
        if force_kill {
            sys::kill_namespace()?;
        }
        match sys::wait_any(true)? {
            sys::Reaped::Child { pid, status } => {
                reaped.record(pid, status);
                if Instant::now() >= deadline {
                    return match sys::wait_any(true)? {
                        sys::Reaped::Child { pid, status } => {
                            reaped.record(pid, status);
                            Ok(DrainOutcome::DeadlineExpired)
                        }
                        sys::Reaped::NotYet => Ok(DrainOutcome::DeadlineExpired),
                        sys::Reaped::NoChildren => Ok(DrainOutcome::Drained),
                    };
                }
            }
            sys::Reaped::NotYet => {
                if Instant::now() >= deadline {
                    return Ok(DrainOutcome::DeadlineExpired);
                }
                std::thread::sleep(REAPER_POLL);
            }
            sys::Reaped::NoChildren => return Ok(DrainOutcome::Drained),
        }
    }
}

impl CgroupCleanup {
    fn spawn(executable: &Path, membership: &str) -> io::Result<Self> {
        let (reader, writer) = io::pipe()?;
        let (mut ready_reader, ready_writer) = io::pipe()?;
        let mut command = Command::new(executable);
        command
            .arg(CGROUP_CLEANUP_ARG)
            .arg(membership)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(reader))
            .stdout(Stdio::from(ready_writer))
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|error| io::Error::other(format!("spawn cgroup cleanup helper: {error}")))?;
        drop(command);
        let mut readiness = [0_u8; 1];
        let armed = ready_reader
            .read_exact(&mut readiness)
            .and_then(|()| {
                if readiness == CGROUP_CLEANUP_READY {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cgroup cleanup helper returned an invalid readiness byte",
                    ))
                }
            });
        drop(ready_reader);
        if let Err(error) = armed {
            drop(writer);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                error.kind(),
                format!("arm cgroup cleanup helper: {error}"),
            ));
        }
        Ok(Self {
            child: Some(child),
            keepalive: Some(writer),
        })
    }

    fn keepalive_descriptor(&self) -> io::Result<u32> {
        let descriptor = self
            .keepalive
            .as_ref()
            .ok_or_else(|| io::Error::other("cgroup cleanup keepalive is already closed"))?
            .as_raw_fd();
        u32::try_from(descriptor)
            .map_err(|error| io::Error::other(format!("invalid cleanup descriptor: {error}")))
    }

    fn close_and_wait(&mut self) -> io::Result<()> {
        drop(self.keepalive.take());
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = loop {
            match child.wait() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => break result?,
            }
        };
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "cgroup cleanup helper exited unsuccessfully: {status}"
            )))
        }
    }

    fn finish(mut self) -> io::Result<()> {
        self.close_and_wait()
    }
}

impl Drop for CgroupCleanup {
    fn drop(&mut self) {
        let _ = self.close_and_wait();
    }
}

impl ManagedCgroup {
    fn disabled() -> Self {
        Self {
            instance: None,
            cleanup: None,
        }
    }

    fn create(
        executable: &Path,
        instance: &str,
        limits: ResolvedResourceLimits,
        identity: Identity,
    ) -> io::Result<Self> {
        let membership = cgroup::membership_for_instance(instance)?;
        let cleanup = CgroupCleanup::spawn(executable, &membership)?;
        match cgroup::Instance::create(instance, limits, identity.uid, identity.gid) {
            Ok(instance) => Ok(Self {
                instance: Some(instance),
                cleanup: Some(cleanup),
            }),
            Err(error) => match cleanup.finish() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}; cgroup cleanup helper: {cleanup}"
                ))),
            },
        }
    }

    fn membership(&self) -> io::Result<&str> {
        if self.instance.is_none() && self.cleanup.is_none() {
            return Ok(NO_CGROUP_MEMBERSHIP);
        }
        self.instance
            .as_ref()
            .map(cgroup::Instance::membership)
            .ok_or_else(|| io::Error::other("application cgroup is already finished"))
    }

    fn keepalive_descriptor(&self) -> io::Result<Option<u32>> {
        match self.cleanup.as_ref() {
            Some(cleanup) => cleanup.keepalive_descriptor().map(Some),
            None if self.instance.is_none() => Ok(None),
            None => Err(io::Error::other(
                "cgroup cleanup helper is already finished",
            )),
        }
    }

    fn attach(&self, pid: u32) -> io::Result<()> {
        if self.instance.is_none() && self.cleanup.is_none() {
            return Ok(());
        }
        self.instance
            .as_ref()
            .ok_or_else(|| io::Error::other("application cgroup is already finished"))?
            .attach(pid)
    }

    fn finish(mut self) -> io::Result<Option<cgroup::Report>> {
        if self.instance.is_none() && self.cleanup.is_none() {
            return Ok(None);
        }
        let cgroup_result = self
            .instance
            .take()
            .ok_or_else(|| io::Error::other("application cgroup is already finished"))?
            .report_and_release();
        let cleanup_result = self
            .cleanup
            .take()
            .ok_or_else(|| io::Error::other("cgroup cleanup helper is already finished"))?
            .finish();
        match (cgroup_result, cleanup_result) {
            (Ok(report), Ok(())) => Ok(Some(report)),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(cgroup), Err(cleanup)) => Err(io::Error::other(format!(
                "{cgroup}; cgroup cleanup helper: {cleanup}"
            ))),
        }
    }
}

impl Drop for ManagedCgroup {
    fn drop(&mut self) {
        drop(self.instance.take());
        drop(self.cleanup.take());
    }
}

pub fn launch_application(application: LaunchPlan) -> io::Result<()> {
    let outside_identity = current_identity()?;
    if outside_identity.uid == 0 || outside_identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail requires the nonzero application identity",
        ));
    }
    if outside_identity.uid != application.outside_uid
        || outside_identity.gid != application.outside_gid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail launch identity changed after authority resolution",
        ));
    }
    let inside_identity = Identity {
        uid: application.inside_uid,
        gid: application.inside_gid,
    };
    let before = NamespaceSnapshot::read()?;
    let token = random_token()?;
    let executable = std::env::current_exe()?;

    let instance = instance_name(&application.name)?;
    let application_cgroup = if application.enforce_cgroup {
        ManagedCgroup::create(
            &executable,
            &instance,
            application.resources,
            outside_identity,
        )?
    } else {
        ManagedCgroup::disabled()
    };
    let cleanup_descriptor = application_cgroup.keepalive_descriptor()?;

    // Phase one. What is load-bearing is that it precedes the SPAWN: the
    // pending registration has to exist before any process that could connect
    // from inside the jail does. Before the unshare as well, which §D
    // specifies and which buys the cheap half — a refused registration costs
    // no namespaces, no mounts and no child to reap.
    //
    // It is NOT because the broker would otherwise fail to see this pid. A
    // draft of this comment said so and it is false: `unshare(CLONE_NEWPID)`
    // does not move the caller, the identity map keeps this process at uid
    // 1000, a pathname AF_UNIX socket is indifferent to a new network
    // namespace, and stage 1 keeps the old root — only stage 2 pivots. A
    // registration opened after the unshare would name exactly the same pid,
    // which is also why phase two below can open a fresh connection at all.
    // Recorded because two reviewers had to find it.
    //
    // The connection is opened and dropped rather than held. The descriptor
    // sweep preserves only the cleanup keepalive, so no connection survives
    // into phase two. That is the real reason §D compares the same PROCESS
    // across the phases rather than the same connection.
    //
    // No predeclared service names yet. §D carries them for app-local
    // activation, which lands with the activation listener; an empty list is
    // the honest statement that this instance may own none.
    let registration = crate::bus::register(
        &application.bus_socket,
        outside_identity.uid,
        &instance,
        &application.name,
        &[],
    )
    .map_err(|e| io::Error::other(format!("register jail instance {instance:?}: {e}")))?;

    if application.host_mode {
        writeln!(io::stderr(), "{HOST_DEGRADATION_CGROUP}")?;
        writeln!(io::stderr(), "{HOST_DEGRADATION_WAYLAND}")?;
    }

    let launch_result = (|| -> io::Result<()> {
        sys::unshare_namespaces(true).map_err(|error| {
            if application.host_mode {
                io::Error::new(
                    error.kind(),
                    format!("host mode requires namespace confinement: {error}"),
                )
            } else {
                error
            }
        })?;
        install_launch_identity_maps(
            inside_identity,
            outside_identity,
            application.host_mode,
        )?;
        NamespaceSnapshot::read()?.require_all_changed(&before)?;
        sys::bring_up_loopback()
            .map_err(|e| io::Error::other(format!("bring up isolated loopback: {e}")))?;
        close_inherited_descriptors(cleanup_descriptor)?;
        prepare_mount_plan(inside_identity, &executable, Some(&application))?;
        prepare_capability_bridge()?;

        let (proof_reader, mut proof_writer) = io::pipe()?;
        let (mut stage2_error, stage2_error_writer) = io::pipe()?;
        let stage2_arguments = stage2_launch_arguments(
            &token,
            LaunchIdentityMap {
                inside: inside_identity,
                outside: outside_identity,
            },
            &application.entry,
            &application.environment,
            &application.filesystems,
            Stage2ResourceBinding {
                limits: application.resources,
                membership: application_cgroup.membership()?,
            },
            &application.arguments,
        );
        let mut command = Command::new(executable);
        command
            .args(&stage2_arguments)
            .env_clear()
            .stdin(Stdio::from(proof_reader))
            .stdout(Stdio::null())
            .stderr(Stdio::from(stage2_error_writer));
        let mut child = command.spawn()?;
        drop(command);

        if let Err(error) = require_child_pid_namespace_changed(&before, child.id()) {
            let _ = child.kill();
            let status = child.wait();
            drop(proof_writer);
            let diagnostic = match read_launch_diagnostic(&mut stage2_error) {
                Ok(response) if response.trim().is_empty() => String::new(),
                Ok(response) => format!("; diagnostic: {}", response.trim()),
                Err(read_error) => format!("; diagnostic unavailable: {read_error}"),
            };
            let status = match status {
                Ok(status) => status.to_string(),
                Err(wait_error) => format!("wait failed: {wait_error}"),
            };
            return Err(io::Error::other(format!(
                "verify launch-stage PID namespace: {error}; stage 2 {status}{diagnostic}"
            )));
        }
        if let Err(error) = application_cgroup.attach(child.id()) {
            let _ = child.kill();
            let status = child.wait();
            drop(proof_writer);
            let diagnostic = match read_launch_diagnostic(&mut stage2_error) {
                Ok(response) if response.trim().is_empty() => String::new(),
                Ok(response) => format!("; diagnostic: {}", response.trim()),
                Err(read_error) => format!("; diagnostic unavailable: {read_error}"),
            };
            let status = match status {
                Ok(status) => status.to_string(),
                Err(wait_error) => format!("wait failed: {wait_error}"),
            };
            return Err(io::Error::other(format!(
                "attach launch-stage PID to application cgroup: {error}; stage 2 {status}{diagnostic}"
            )));
        }
        // Phase two, and it comes BEFORE the proof write on purpose. That write is
        // what releases stage 2, and `Command::spawn` has already returned with
        // stage 2 runnable — so completing afterwards would leave a window in
        // which the application connects while its registration is still pending.
        // The broker refuses a strict descendant of a pending registrant, which is
        // right, and it fixes identity AT ACCEPT, so such a connection is denied
        // for its whole life however quickly the registration then completes.
        //
        // A failed completion kills the jail rather than launching one the broker
        // has no record of: an unregistered application resolves `Unconfined`,
        // which is full portal access for the one process that is certainly
        // confined. This is §D's "stage 1 refuses to proceed without the token",
        // placed where it is actually enforceable.
        if let Err(error) = crate::bus::complete(
            &application.bus_socket,
            outside_identity.uid,
            &registration,
            child.id(),
        ) {
            let _ = child.kill();
            let status = child.wait();
            drop(proof_writer);
            let diagnostic = match read_launch_diagnostic(&mut stage2_error) {
                Ok(response) if response.trim().is_empty() => String::new(),
                Ok(response) => format!("; diagnostic: {}", response.trim()),
                Err(read_error) => format!("; diagnostic unavailable: {read_error}"),
            };
            let status = match status {
                Ok(status) => status.to_string(),
                Err(wait_error) => format!("wait failed: {wait_error}"),
            };
            return Err(io::Error::other(format!(
                "complete jail instance {instance:?}: {error}; stage 2 {status}{diagnostic}"
            )));
        }

        if let Err(error) = proof_writer.write_all(&token) {
            let _ = child.kill();
            let status = child.wait();
            drop(proof_writer);
            let diagnostic = match read_launch_diagnostic(&mut stage2_error) {
                Ok(response) if response.trim().is_empty() => String::new(),
                Ok(response) => format!("; diagnostic: {}", response.trim()),
                Err(read_error) => format!("; diagnostic unavailable: {read_error}"),
            };
            let status = match status {
                Ok(status) => status.to_string(),
                Err(wait_error) => format!("wait failed: {wait_error}"),
            };
            return Err(io::Error::other(format!(
                "write launch-stage proof: {error}; stage 2 {status}{diagnostic}"
            )));
        }
        let response = match read_launch_diagnostic(&mut stage2_error) {
            Ok(response) => response,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let status = child.wait()?;
        drop(proof_writer);
        if !status.success() {
            let detail = response.trim();
            if !detail.is_empty() {
                return Err(io::Error::other(format!(
                    "application jail exited unsuccessfully: {status}: {detail}"
                )));
            }
            return Err(io::Error::other(format!(
                "application jail exited unsuccessfully: {status}"
            )));
        }
        if !response.is_empty() {
            return Err(io::Error::other(format!(
                "successful application jail returned a diagnostic: {:?}",
                response.trim()
            )));
        }
        Ok(())
    })();
    let cgroup_result = application_cgroup.finish();
    match (launch_result, cgroup_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(Some(report))) => Err(io::Error::other(format!(
            "{error}; application cgroup diagnostics: {}",
            report.diagnostic()
        ))),
        (Err(error), Ok(None)) => Err(error),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "application exited successfully but cgroup cleanup failed: {error}"
        ))),
        (Err(launch), Err(cgroup)) => Err(io::Error::other(format!(
            "{launch}; application cgroup cleanup: {cgroup}"
        ))),
    }
}

fn read_launch_diagnostic(reader: &mut impl Read) -> io::Result<String> {
    let mut response = Vec::new();
    reader
        .take((STAGE2_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > STAGE2_OUTPUT_LIMIT {
        return Err(io::Error::other(
            "launch-stage diagnostic exceeded 4096 bytes",
        ));
    }
    String::from_utf8(response)
        .map_err(|e| io::Error::other(format!("launch-stage diagnostic is not UTF-8: {e}")))
}

pub fn write_standard_filter() -> io::Result<()> {
    seccomp::write_standard_filter(io::stdout().lock())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;
    use crate::authority::{
        mount_identities_outside_allowed_home, mount_tree_identities,
        require_grant_mount_identities, MountIdentity,
    };
    use std::io::BufRead;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory() -> io::Result<PathBuf> {
        loop {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "td-jail-write-probe-test-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The ceiling `td-busd` declares for one of the two names this module
    /// sends it, read out of that crate's own source.
    ///
    /// The two crates are separate dependency-free locks, so neither can name
    /// the other's constant. Restating the number here would be the same
    /// assumption with a comment on it; reading it is a check. The file is
    /// reached under `#[cfg(test)]` only, so the recipe — which stages
    /// `src/*.rs` and nothing else — never expands it into the target build.
    fn broker_ceiling(source: &str, name: &str) -> usize {
        let declaration = format!("const {name}: usize = ");
        let at = source
            .find(&declaration)
            .unwrap_or_else(|| panic!("td-busd no longer declares {name}"));
        let rest = &source[at + declaration.len()..];
        let end = rest
            .find(';')
            .unwrap_or_else(|| panic!("td-busd's {name} declaration is unterminated"));
        rest[..end]
            .trim()
            .replace('_', "")
            .parse()
            .unwrap_or_else(|_| panic!("td-busd's {name} is not a literal this can read"))
    }

    /// §D's instance name is the application's own name and a fresh suffix.
    ///
    /// Fresh matters: the broker refuses a name that is already registered, so
    /// a suffix that repeated would make the second window of a program fail
    /// to launch. That is why it is not the pid — a pid is unique only among
    /// live processes, and this module spends the rest of its length not
    /// relying on that.
    #[test]
    fn an_instance_name_is_the_application_and_a_fresh_suffix() {
        let one = instance_name("firefox").unwrap();
        let two = instance_name("firefox").unwrap();
        assert!(one.starts_with("firefox-"), "{one}");
        assert_eq!(one.len(), "firefox-".len() + INSTANCE_SUFFIX_LEN * 2);
        assert_ne!(one, two, "two launches were given the same instance name");
        for name in [&one, &two] {
            let suffix = &name["firefox-".len()..];
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
                "the suffix is not lowercase hex: {suffix}"
            );
        }
    }

    /// Every instance name this module can build is one the broker's grammar
    /// accepts.
    ///
    /// The predicate is restated from td-busd's `valid_instance_name` rather
    /// than imported — separate locks — and the ceilings the two must agree on
    /// are read out of td-busd's source in the next test rather than restated.
    ///
    /// The application names are chosen for the ways this could go wrong: one
    /// ending in a dot and one starting with one, since a `..` anywhere is
    /// refused and appending to a name is where one could appear; the longest
    /// name `authority` will accept; and the shortest.
    #[test]
    fn an_instance_name_satisfies_the_brokers_grammar() {
        let longest = "a".repeat(crate::authority::MAX_APPLICATION_NAME_BYTES);
        let applications = [
            "firefox",
            "td-jail-fixture",
            "a",
            "a.",
            ".a",
            "x.y_z-1",
            &longest,
        ];
        for application in applications {
            for _ in 0..64 {
                let name = instance_name(application).unwrap();
                assert!(!name.is_empty());
                assert_ne!(name, ".");
                assert!(!name.contains(".."), "{name}");
                assert!(
                    name.bytes()
                        .all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.')),
                    "{name}"
                );
            }
        }
    }

    /// And the broker's ceilings admit every name this module can build.
    ///
    /// Both ways of getting this wrong fail at launch rather than at build:
    /// a name too long is refused by the broker, which is a boot-time failure
    /// for the fixture and a dead application for anyone else. Neither ceiling
    /// is reachable from here, so this reads them.
    ///
    /// `cargo test` for this crate is reached whenever td-busd changes:
    /// `affected.rs` narrows the cargo table only when every changed path is
    /// inside an unembedded crate, and td-busd is embedded by a recipe, so a
    /// change to it takes the whole table.
    #[test]
    fn the_brokers_ceilings_admit_every_name_this_module_can_build() {
        const BROKER: &str = include_str!("../../td-busd/src/transport.rs");
        let application = crate::authority::MAX_APPLICATION_NAME_BYTES;
        assert!(
            application <= broker_ceiling(BROKER, "MAX_APPLICATION_ID"),
            "an application name this crate accepts is longer than an app id \
             td-busd accepts, so those applications cannot launch"
        );
        assert!(
            application + 1 + INSTANCE_SUFFIX_LEN * 2
                <= broker_ceiling(BROKER, "MAX_INSTANCE_NAME"),
            "the longest instance name this module builds is longer than an \
             instance name td-busd accepts, so those applications cannot launch"
        );
    }

    #[test]
    fn public_mode_is_only_probe_and_internal_modes_are_exact() {
        assert_eq!(parse_mode(args(&[PROBE_ARG])).unwrap(), Mode::Probe);
        assert!(parse_mode(args(&[])).is_err());
        assert!(parse_mode(args(&["firefox"])).is_err());
        assert!(parse_mode(args(&[PROBE_ARG, "extra"])).is_err());
        assert_eq!(parse_mode(args(&[FILTER_ARG])).unwrap(), Mode::WriteFilter);
        assert!(parse_mode(args(&[FILTER_ARG, "extra"])).is_err());
        assert_eq!(
            parse_mode(args(&[
                APPLICATION_SESSION_ARG,
                "42",
                "/bin/firefox",
                "--safe-mode",
            ]))
            .unwrap(),
            Mode::ApplicationSession {
                parent: 42,
                argv0: OsString::from("/bin/firefox"),
                arguments: vec![OsString::from("--safe-mode")],
            }
        );
        assert!(parse_mode(args(&[APPLICATION_SESSION_ARG])).is_err());
        assert!(parse_mode(args(&[APPLICATION_SESSION_ARG, "0", "/bin/firefox"])).is_err());
        assert!(parse_mode(args(&[APPLICATION_SESSION_ARG, "+42", "/bin/firefox"])).is_err());
        assert!(parse_mode(args(&[APPLICATION_SESSION_ARG, " 42", "/bin/firefox"])).is_err());
        assert!(parse_mode(args(&[APPLICATION_SESSION_ARG, "42"])).is_err());
        let membership = "/td-user-1000/app-0123456789abcdef";
        assert_eq!(
            parse_mode(args(&[CGROUP_CLEANUP_ARG, membership])).unwrap(),
            Mode::CgroupCleanupBootstrap {
                membership: membership.into(),
            }
        );
        assert!(parse_mode(args(&[CGROUP_CLEANUP_ARG])).is_err());
        assert!(parse_mode(args(&[CGROUP_CLEANUP_ARG, "/elsewhere/app"])).is_err());
        assert!(parse_mode(args(&[CGROUP_CLEANUP_ARG, membership, "extra"])).is_err());
        assert_eq!(
            parse_mode(args(&[CGROUP_CLEANUP_WATCH_ARG, membership])).unwrap(),
            Mode::CgroupCleanupWatcher {
                membership: membership.into(),
            }
        );
        assert!(parse_mode(args(&[CGROUP_CLEANUP_WATCH_ARG])).is_err());
        assert!(parse_mode(args(&[
            CGROUP_CLEANUP_WATCH_ARG,
            "/elsewhere/app"
        ]))
        .is_err());
        assert!(parse_mode(args(&[
            CGROUP_CLEANUP_WATCH_ARG,
            membership,
            "extra"
        ]))
        .is_err());
        assert_eq!(
            parse_mode(args(&[REAPER_CHILD_ARG])).unwrap(),
            Mode::ReaperChild
        );
        assert_eq!(
            parse_mode(args(&[REAPER_ORPHAN_ARG])).unwrap(),
            Mode::ReaperOrphan
        );
        assert_eq!(
            parse_mode(args(&[SURVIVOR_CHILD_ARG])).unwrap(),
            Mode::SurvivorChild
        );
        assert_eq!(
            parse_mode(args(&[SURVIVOR_ORPHAN_ARG])).unwrap(),
            Mode::SurvivorOrphan
        );
        assert!(parse_mode(args(&[REAPER_CHILD_ARG, "extra"])).is_err());
        assert!(parse_mode(args(&[REAPER_ORPHAN_ARG, "extra"])).is_err());
        assert!(parse_mode(args(&[SURVIVOR_CHILD_ARG, "extra"])).is_err());
        assert!(parse_mode(args(&[SURVIVOR_ORPHAN_ARG, "extra"])).is_err());
    }

    #[test]
    fn survivor_reap_summary_does_not_grow_with_the_child_table() {
        let mut reaped = SurvivorReap::default();
        reaped.record(2, sys::SIGTERM);
        assert_eq!(
            reaped,
            SurvivorReap {
                count: 1,
                sole_child: Some((2, sys::SIGTERM)),
            }
        );
        reaped.record(3, sys::SIGKILL);
        assert_eq!(
            reaped,
            SurvivorReap {
                count: 2,
                sole_child: None,
            }
        );
    }

    #[test]
    fn cgroup_cleanup_keepalive_accepts_only_eof() {
        wait_for_parent_end(&mut io::empty()).unwrap();
        let error = wait_for_parent_end(&mut io::Cursor::new([1_u8])).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn survivor_cleanup_escalates_and_preserves_the_hard_phase_result() {
        let terminated = std::cell::Cell::new(false);
        let calls = std::cell::RefCell::new(Vec::new());
        let outcomes = std::cell::RefCell::new(std::collections::VecDeque::from([
            DrainOutcome::DeadlineExpired,
            DrainOutcome::Drained,
        ]));
        let reaped = terminate_and_reap_survivors_with(
            || {
                terminated.set(true);
                Ok(())
            },
            |timeout, reaped, force_kill| {
                calls.borrow_mut().push((timeout, force_kill));
                if force_kill {
                    reaped.record(3, sys::SIGKILL);
                }
                Ok(outcomes.borrow_mut().pop_front().unwrap())
            },
        )
        .unwrap();
        assert!(terminated.get());
        assert_eq!(
            calls.into_inner(),
            [
                (SURVIVOR_TERM_TIMEOUT, false),
                (SURVIVOR_KILL_TIMEOUT, true),
            ]
        );
        assert_eq!(
            reaped,
            SurvivorReap {
                count: 1,
                sole_child: Some((3, sys::SIGKILL)),
            }
        );
    }

    #[test]
    fn survivor_cleanup_fails_when_both_deadlines_expire() {
        let error = terminate_and_reap_survivors_with(
            || Ok(()),
            |_, _, _| Ok(DrainOutcome::DeadlineExpired),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("SIGKILL deadline before PID 1 observed ECHILD"));
    }

    #[test]
    fn stage2_token_round_trips_and_is_strict() {
        let mut token = [0_u8; TOKEN_LEN];
        for (index, byte) in token.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        let encoded = encode_token(&token);
        assert_eq!(decode_token(&OsString::from(&encoded)).unwrap(), token);
        assert!(decode_token(&OsString::from("00")).is_err());
        assert!(decode_token(&OsString::from("G0".repeat(TOKEN_LEN))).is_err());
        assert!(parse_mode(args(&[
            STAGE2_ARG,
            &encoded,
            "1000",
            "1000",
            "1000",
            "1000",
            STAGE2_PROBE_ARG,
        ]))
        .is_ok());
        assert!(matches!(
            parse_mode(args(&[
                STAGE2_ARG,
                &encoded,
                "1000",
                "1000",
                "1234",
                "2345",
                STAGE2_LAUNCH_ARG,
                "/app/bin/app",
                STAGE2_ENVIRONMENT_ARG,
                "6",
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/run/user/1000/bus",
                "FLATPAK_ID",
                "org.td.App",
                "GLIBC_TUNABLES",
                "glibc.malloc.perturb=1",
                "HOME",
                "/home/td",
                "WAYLAND_DISPLAY",
                "wayland-0",
                "XDG_RUNTIME_DIR",
                "/run/user/1000",
                STAGE2_FILESYSTEMS_ARG,
                "1",
                "/home/td/Downloads",
                "rw-dir",
                STAGE2_RESOURCES_ARG,
                "50331648",
                "67108864",
                "32",
                "/td-user-1000/app-0123456789abcdef",
                STAGE2_ARGUMENTS_ARG,
                "--flag",
            ]))
            .unwrap(),
            Mode::Stage2 {
                action: Stage2Action::Launch {
                    entry,
                    environment,
                    filesystems,
                    resources,
                    cgroup_membership,
                    arguments,
                },
                ..
            } if entry == "/app/bin/app"
                && environment.first() == Some(&(
                    OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                    OsString::from("unix:path=/run/user/1000/bus"),
                ))
                && filesystems == [Stage2Filesystem {
                    target: PathBuf::from("/home/td/Downloads"),
                    read_only: false,
                    source_kind: FilesystemSourceKind::Directory,
                }]
                && resources == ResolvedResourceLimits {
                    memory_high_bytes: 50_331_648,
                    memory_max_bytes: 67_108_864,
                    pids_max: 32,
                }
                && cgroup_membership == "/td-user-1000/app-0123456789abcdef"
                && arguments == [OsString::from("--flag")]
        ));
        assert!(parse_mode(args(&[STAGE2_ARG, &encoded])).is_err());
        assert!(parse_mode(args(&[STAGE2_ARG, &encoded, "1000", "1000", "extra"])).is_err());
        assert!(parse_mode(args(&[
            STAGE2_ARG,
            &encoded,
            "1000",
            "1000",
            "1000",
            "1000",
            STAGE2_LAUNCH_ARG,
            "/usr/bin/app",
            STAGE2_ENVIRONMENT_ARG,
            "0",
            STAGE2_FILESYSTEMS_ARG,
            "0",
            STAGE2_ARGUMENTS_ARG,
        ]))
        .is_err());
        assert!(parse_mode(args(&[
            STAGE2_ARG,
            &encoded,
            "1000",
            "1000",
            "1000",
            "1000",
            STAGE2_LAUNCH_ARG,
            "/app/bin/app",
            STAGE2_ENVIRONMENT_ARG,
            "4",
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/run/user/1000/bus",
            "WAYLAND_DISPLAY",
            "wayland-0",
            "HOME",
            "/home/td",
            "XDG_RUNTIME_DIR",
            "/run/user/1000",
            STAGE2_FILESYSTEMS_ARG,
            "0",
            STAGE2_ARGUMENTS_ARG,
        ]))
        .is_err());
    }

    #[test]
    fn stage2_launch_emitter_round_trips_through_the_parser() {
        let token = [7_u8; TOKEN_LEN];
        let identity = Identity {
            uid: 1000,
            gid: 1000,
        };
        let outside_identity = Identity {
            uid: 1234,
            gid: 2345,
        };
        let environment = vec![
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("unix:path=/run/user/1000/bus"),
            ),
            (OsString::from("FLATPAK_ID"), OsString::from("org.td.App")),
            (OsString::from("HOME"), OsString::from("/home/td")),
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-0"),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                OsString::from("/run/user/1000"),
            ),
        ];
        let arguments = vec![OsString::from("--flag")];
        let resources = ResolvedResourceLimits {
            memory_high_bytes: 50_331_648,
            memory_max_bytes: 67_108_864,
            pids_max: 32,
        };
        let membership = "/td-user-1000/app-0123456789abcdef";
        let filesystems = vec![FilesystemGrant {
            source: PathBuf::from("/host/downloads"),
            target: PathBuf::from("/home/td/Downloads"),
            read_only: false,
            source_kind: FilesystemSourceKind::Directory,
            source_device: 1,
            source_inode: 2,
        }];
        let emitted = stage2_launch_arguments(
            &token,
            LaunchIdentityMap {
                inside: identity,
                outside: outside_identity,
            },
            "/app/bin/app",
            &environment,
            &filesystems,
            Stage2ResourceBinding {
                limits: resources,
                membership,
            },
            &arguments,
        );
        assert_eq!(
            parse_mode(emitted.into_iter()).unwrap(),
            Mode::Stage2 {
                token,
                identity,
                outside_identity,
                action: Stage2Action::Launch {
                    entry: "/app/bin/app".into(),
                    environment,
                    filesystems: vec![Stage2Filesystem {
                        target: PathBuf::from("/home/td/Downloads"),
                        read_only: false,
                        source_kind: FilesystemSourceKind::Directory,
                    }],
                    resources,
                    cgroup_membership: membership.into(),
                    arguments,
                },
            }
        );
    }

    #[test]
    fn resource_probe_mode_is_named_and_bounded() {
        assert_eq!(
            parse_mode(args(&[RESOURCE_PROBE_ARG, "td-jail-fixture"])).unwrap(),
            Mode::ResourceProbe {
                application: "td-jail-fixture".into(),
            }
        );
        assert!(parse_mode(args(&[RESOURCE_PROBE_ARG])).is_err());
        assert!(parse_mode(args(&[RESOURCE_PROBE_ARG, "bad/name"])).is_err());
        assert!(parse_mode(args(&[RESOURCE_PROBE_ARG, "app", "extra"])).is_err());
    }

    #[test]
    fn writable_probe_never_replaces_an_existing_path() {
        let directory = temporary_directory().unwrap();
        let directory_text = directory.to_str().unwrap();
        let token = [9_u8; TOKEN_LEN];
        let probe = writable_probe_path(directory_text, &token);
        require_writable_directory(directory_text, &token).unwrap();
        assert!(fs::read_dir(&directory).unwrap().next().is_none());

        fs::write(&probe, b"application-owned").unwrap();
        let error = require_writable_directory(directory_text, &token).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&probe).unwrap(), b"application-owned");
        fs::remove_file(&probe).unwrap();

        let target = directory.join("application-target");
        fs::write(&target, b"symlink-target").unwrap();
        symlink(&target, &probe).unwrap();
        let error = require_writable_directory(directory_text, &token).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::symlink_metadata(&probe)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"symlink-target");

        fs::remove_file(&probe).unwrap();
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn dropping_a_spawned_launch_command_releases_its_diagnostic_writer() {
        let (mut reader, writer) = io::pipe().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::from(writer));
        let mut child = command.spawn().unwrap();
        drop(command);
        assert!(child.wait().unwrap().success());
        let mut response = Vec::new();
        reader.read_to_end(&mut response).unwrap();
        assert!(response.is_empty());
    }

    #[test]
    fn launch_diagnostics_are_bounded_utf8() {
        let mut valid = &b"stage failed\n"[..];
        assert_eq!(read_launch_diagnostic(&mut valid).unwrap(), "stage failed\n");

        let mut invalid = &[0xff_u8][..];
        assert!(read_launch_diagnostic(&mut invalid).is_err());

        let oversized = vec![b'x'; STAGE2_OUTPUT_LIMIT + 1];
        let mut oversized = oversized.as_slice();
        assert!(read_launch_diagnostic(&mut oversized).is_err());
    }

    #[test]
    fn writable_probe_preserves_interrupted_launch_residue() {
        let directory = temporary_directory().unwrap();
        let directory_text = directory.to_str().unwrap();
        let stale = writable_probe_path(directory_text, &[8_u8; TOKEN_LEN]);
        fs::write(&stale, b"ok").unwrap();

        require_writable_directory(directory_text, &[9_u8; TOKEN_LEN]).unwrap();
        assert_eq!(fs::read(&stale).unwrap(), b"ok");
        fs::remove_file(&stale).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn writable_probe_ignores_application_owned_reserved_lookalikes() {
        let directory = temporary_directory().unwrap();
        let directory_text = directory.to_str().unwrap();
        let regular = writable_probe_path(directory_text, &[7_u8; TOKEN_LEN]);
        let reserved_directory = writable_probe_path(directory_text, &[8_u8; TOKEN_LEN]);
        fs::write(&regular, b"application-owned").unwrap();
        fs::create_dir(&reserved_directory).unwrap();

        require_writable_directory(directory_text, &[9_u8; TOKEN_LEN]).unwrap();
        assert_eq!(fs::read(&regular).unwrap(), b"application-owned");
        assert!(fs::symlink_metadata(&reserved_directory)
            .unwrap()
            .file_type()
            .is_dir());

        fs::remove_file(&regular).unwrap();
        fs::remove_dir(&reserved_directory).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn status_parser_uses_the_effective_column() {
        let status = "Name:\ttd-jail\nUid:\t1000\t1001\t1002\t1003\nGid:\t10\t11\t12\t13\nCapEff:\t0000000000200000\nNoNewPrivs:\t1\nSeccomp:\t2\n";
        assert_eq!(effective_id(status, "Uid:").unwrap(), 1001);
        assert_eq!(effective_id(status, "Gid:").unwrap(), 11);
        assert_eq!(capability_row(status, "CapEff:").unwrap(), 1 << 21);
        assert_eq!(decimal_status_row(status, "NoNewPrivs:").unwrap(), 1);
        assert_eq!(decimal_status_row(status, "Seccomp:").unwrap(), 2);
        assert!(effective_id(status, "Groups:").is_err());
        assert!(capability_row("Name:\ttd-jail\n", "CapEff:").is_err());
        assert!(decimal_status_row(status, "Missing:").is_err());
    }

    #[test]
    fn identity_map_accepts_a_distinct_host_identity_only_when_expected() {
        let map = "1000 1234 1\n";
        validate_single_map("uid_map", map, 1000, Some(1234)).unwrap();
        validate_single_map("uid_map", map, 1000, None).unwrap();
        assert!(validate_single_map("uid_map", map, 1000, Some(1000)).is_err());
        assert!(validate_single_map("uid_map", map, 1234, Some(1000)).is_err());
        assert!(validate_single_map("uid_map", "1000 1234 2\n", 1000, None).is_err());
        assert!(validate_single_map(
            "uid_map",
            "1000 1234 1\n2000 2234 1\n",
            1000,
            None,
        )
        .is_err());
    }

    #[test]
    fn proof_comparison_checks_every_byte() {
        let token = [7_u8; TOKEN_LEN];
        assert!(tokens_equal(&token, &token));
        let mut changed = token;
        changed[TOKEN_LEN - 1] = 8;
        assert!(!tokens_equal(&token, &changed));
    }

    #[test]
    fn liveness_watcher_retries_an_interrupted_read() {
        struct InterruptedThenEof {
            reads: usize,
        }

        impl Read for InterruptedThenEof {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads == 1 {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                Ok(0)
            }
        }

        let mut reader = InterruptedThenEof { reads: 0 };
        wait_for_stage1_end(&mut reader);
        assert_eq!(reader.reads, 2);
    }

    #[test]
    fn stage1_liveness_watcher_helper() {
        if std::env::var_os("TD_JAIL_TEST_STAGE1_LIVENESS").is_none() {
            return;
        }
        start_stage1_liveness_watcher().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn stage1_pipe_eof_terminates_the_supervisor() {
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "transition::tests::stage1_liveness_watcher_helper",
                "--nocapture",
            ])
            .env("TD_JAIL_TEST_STAGE1_LIVENESS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        drop(writer);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(125));
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("stage-1 EOF did not terminate the supervisor");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn application_session_child_helper() {
        let Some(parent) = std::env::var_os("TD_JAIL_TEST_APPLICATION_SESSION_CHILD") else {
            return;
        };
        let parent = parent.to_str().unwrap().parse().unwrap();
        contain_application_session(parent).unwrap();
        let containment =
            process_containment(&fs::read_to_string("/proc/self/stat").unwrap()).unwrap();
        writeln!(
            io::stderr(),
            "{} {} {} {}",
            std::process::id(),
            containment.process_group,
            containment.session,
            containment.terminal
        )
        .unwrap();
        io::stderr().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn application_session_parent_helper() {
        if std::env::var_os("TD_JAIL_TEST_APPLICATION_SESSION_PARENT").is_none() {
            return;
        }
        if let Some(parent) =
            std::env::var_os("TD_JAIL_TEST_APPLICATION_SESSION_PARENT_DETACH_FROM")
        {
            contain_application_session(parent.to_str().unwrap().parse().unwrap()).unwrap();
        }
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "transition::tests::application_session_child_helper",
                "--nocapture",
            ])
            .env(
                "TD_JAIL_TEST_APPLICATION_SESSION_CHILD",
                std::process::id().to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut readiness = String::new();
        std::io::BufReader::new(stderr)
            .read_line(&mut readiness)
            .unwrap();
        write!(io::stderr(), "{readiness}").unwrap();
        io::stderr().flush().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn killing_the_waiting_parent_kills_the_detached_application_session() {
        run_application_session_parent_death(false);
    }

    #[test]
    fn a_no_terminal_supervisor_group_remains_the_application_stop_scope() {
        run_application_session_parent_death(true);
    }

    fn run_application_session_parent_death(supervised_group: bool) {
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .args([
                "--exact",
                "transition::tests::application_session_parent_helper",
                "--nocapture",
            ])
            .env("TD_JAIL_TEST_APPLICATION_SESSION_PARENT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if supervised_group {
            command.env(
                "TD_JAIL_TEST_APPLICATION_SESSION_PARENT_DETACH_FROM",
                std::process::id().to_string(),
            );
        }
        let mut parent = command.spawn().unwrap();
        let parent_pid = parent.id();
        let stderr = parent.stderr.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut readiness = String::new();
            let result = std::io::BufReader::new(stderr)
                .read_line(&mut readiness)
                .map(|_| readiness);
            let _ = sender.send(result);
        });
        let readiness = receiver.recv_timeout(Duration::from_secs(2));
        let observed = readiness
            .map_err(|error| io::Error::other(format!("application session readiness: {error}")))
            .and_then(|result| result)
            .and_then(|line| parse_application_session_readiness(&line));
        let before = observed
            .as_ref()
            .ok()
            .map(|(pid, _, _, _)| {
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .and_then(|stat| process_state_and_starttime(&stat))
            })
            .transpose();

        parent.kill().unwrap();
        let _ = parent.wait().unwrap();

        let (pid, process_group, session, terminal) = observed.unwrap();
        let expected = if supervised_group { parent_pid } else { pid };
        assert_eq!(process_group, expected);
        assert_eq!(session, expected);
        assert_eq!(terminal, 0);
        let (_, starttime) = before.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => panic!("read application session: {error}"),
                Ok(stat) => {
                    let (state, current_starttime) = process_state_and_starttime(&stat).unwrap();
                    if state == 'Z' || current_starttime != starttime {
                        break;
                    }
                    if Instant::now() >= deadline {
                        panic!("application session survived its waiting parent");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn parse_application_session_readiness(text: &str) -> io::Result<(u32, u32, u32, i64)> {
        let mut fields = text.split_whitespace();
        let pid = parse_test_field(&mut fields, "pid")?;
        let process_group = parse_test_field(&mut fields, "process group")?;
        let session = parse_test_field(&mut fields, "session")?;
        let terminal = parse_test_field(&mut fields, "terminal")?;
        if fields.next().is_some() {
            return Err(io::Error::other(
                "application session readiness has extra fields",
            ));
        }
        Ok((pid, process_group, session, terminal))
    }

    fn parse_test_field<'a, T: std::str::FromStr>(
        fields: &mut impl Iterator<Item = &'a str>,
        name: &str,
    ) -> io::Result<T> {
        fields
            .next()
            .ok_or_else(|| {
                io::Error::other(format!("application session readiness has no {name}"))
            })?
            .parse()
            .map_err(|_| io::Error::other(format!("invalid application session {name}")))
    }

    fn process_state_and_starttime(stat: &str) -> io::Result<(char, u64)> {
        let (_, fields) = stat
            .rsplit_once(") ")
            .ok_or_else(|| io::Error::other("process stat has no command terminator"))?;
        let mut fields = fields.split_whitespace();
        let state = fields
            .next()
            .and_then(|value| value.chars().next())
            .ok_or_else(|| io::Error::other("process stat has no state"))?;
        let starttime = fields
            .nth(18)
            .ok_or_else(|| io::Error::other("process stat has no starttime"))?
            .parse()
            .map_err(|error| io::Error::other(format!("invalid process starttime: {error}")))?;
        Ok((state, starttime))
    }

    #[test]
    fn device_number_decoder_matches_the_compiled_roster() {
        fn encode(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8)
                | ((major & 0xffff_f000) << 32)
                | (minor & 0xff)
                | ((minor & 0xffff_ff00) << 12)
        }

        for (_, major, minor) in DEVICE_NODES {
            assert_eq!(device_numbers(encode(*major, *minor)), (*major, *minor));
        }
    }

    #[test]
    fn mountinfo_parser_requires_type_and_options() {
        let mountinfo = "1 0 0:1 / / ro,nosuid,nodev - tmpfs tmpfs rw\n\
                         2 1 0:2 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=524288k\n";
        assert!(require_mount(
            mountinfo,
            "/",
            Some("tmpfs"),
            &["ro", "nosuid", "nodev"],
            &["rw"]
        )
        .is_ok());
        assert!(require_mount(mountinfo, "/", Some("proc"), &["ro"], &[]).is_err());
        assert!(require_mount(mountinfo, "/tmp", Some("tmpfs"), &["ro"], &[]).is_err());
        assert!(require_mount(mountinfo, "/tmp", None, &["rw"], &["ro"]).is_ok());
        assert!(require_mount(mountinfo, "/missing", Some("tmpfs"), &[], &[]).is_err());
        assert!(require_mount_super_option(mountinfo, "/tmp", "size=524288k").is_ok());
        assert!(require_mount_super_option(mountinfo, "/tmp", "size=1k").is_err());

        let binds = "1 0 0:1 / / rw - ext4 root rw\n\
                     2 1 0:2 / /run/user/1000 rw - tmpfs run rw\n\
                     3 1 0:2 /td-app/fixture /tmp/td-jail-root/run/user/1000/td-app rw - tmpfs run rw\n";
        assert!(require_bind_source(
            binds,
            Path::new("/run/user/1000/td-app/fixture"),
            Path::new("/tmp/td-jail-root/run/user/1000/td-app"),
        )
        .is_ok());
        assert!(require_bind_source(
            &binds.replace("/td-app/fixture", "/td-app/other"),
            Path::new("/run/user/1000/td-app/fixture"),
            Path::new("/tmp/td-jail-root/run/user/1000/td-app"),
        )
        .is_err());

        let canonical_home = "1 0 0:1 / / rw - erofs root rw\n\
                              2 1 0:2 / /var rw - btrfs var rw\n\
                              3 1 0:2 /home/tester/state /tmp/td-jail-root/home/td rw - btrfs var rw\n";
        assert!(require_bind_source(
            canonical_home,
            Path::new("/var/home/tester/state"),
            Path::new("/tmp/td-jail-root/home/td"),
        )
        .is_ok());
        assert!(require_bind_source(
            canonical_home,
            Path::new("/home/tester/state"),
            Path::new("/tmp/td-jail-root/home/td"),
        )
        .is_err());

        let escaped = "1 0 0:1 / / rw - tmpfs root rw\n\
                       2 1 0:3 /path\\040with\\134slash /source\\040dir rw - tmpfs tmpfs rw\n\
                       3 1 0:3 /path\\040with\\134slash /target rw - tmpfs tmpfs rw\n";
        assert!(require_bind_source(
            escaped,
            Path::new("/source dir"),
            Path::new("/target"),
        )
        .is_ok());
        assert!(decode_mountinfo_path("/bad\\777").is_err());

        let shadowed = "1 0 0:1 / / rw - tmpfs root rw\n\
                        2 1 8:1 /hidden /a/b rw - ext4 hidden rw\n\
                        3 1 0:3 / /a rw - tmpfs visible rw\n";
        assert_eq!(
            mount_identity_for_path(shadowed, Path::new("/a/b/file")).unwrap(),
            MountIdentity {
                device: "0:3".into(),
                root: PathBuf::from("/b/file"),
            }
        );
        let visible_child = format!(
            "{shadowed}4 3 0:4 /child /a/b rw - tmpfs child rw\n"
        );
        assert_eq!(
            mount_identity_for_path(&visible_child, Path::new("/a/b/file")).unwrap(),
            MountIdentity {
                device: "0:4".into(),
                root: PathBuf::from("/child/file"),
            }
        );
    }

    #[test]
    fn grant_mount_identity_refuses_reserved_and_other_home_aliases() {
        let base = "1 0 8:1 / / rw - ext4 root rw\n\
                    2 1 0:2 / /run rw - tmpfs run rw\n\
                    3 2 0:3 / /run/shm rw - tmpfs shm rw\n\
                    4 1 0:4 / /home/other rw - btrfs other rw\n\
                    5 1 0:5 / /home/tester/Media rw - btrfs own rw\n";
        let source = Path::new("/mnt/grant");
        let allowed_home = Path::new("/home/tester");
        let home_roots = [PathBuf::from("/home")];

        let check = |mountinfo: &str| {
            require_grant_mount_identities(
                source,
                &mount_tree_identities(mountinfo, source).unwrap(),
                &mount_tree_identities(mountinfo, Path::new("/run")).unwrap(),
                &mount_tree_identities(mountinfo, Path::new("/home")).unwrap(),
                &mount_tree_identities(mountinfo, allowed_home).unwrap(),
                &mount_identities_outside_allowed_home(
                    mountinfo,
                    &home_roots,
                    allowed_home,
                )
                .unwrap(),
            )
        };

        let reserved_alias =
            format!("{base}6 1 0:3 / /mnt/grant/nested rw - tmpfs shm rw\n");
        assert!(require_grant_mount_identities(
            source,
            &mount_tree_identities(&reserved_alias, source).unwrap(),
            &mount_tree_identities(&reserved_alias, Path::new("/run")).unwrap(),
            &mount_tree_identities(&reserved_alias, Path::new("/home")).unwrap(),
            &mount_tree_identities(&reserved_alias, allowed_home).unwrap(),
            &mount_identities_outside_allowed_home(
                &reserved_alias,
                &home_roots,
                allowed_home,
            )
            .unwrap(),
        )
        .unwrap_err()
        .to_string()
        .contains("aliases a reserved mount"));

        let other_home =
            format!("{base}6 1 0:4 / /mnt/grant/nested rw - btrfs other rw\n");
        assert!(check(&other_home)
            .unwrap_err()
            .to_string()
            .contains("aliases another home mount"));

        let mixed_homes = format!(
            "{base}6 1 0:4 / /mnt/grant/other rw - btrfs other rw\n\
             7 1 0:5 / /mnt/grant/own rw - btrfs own rw\n"
        );
        assert!(check(&mixed_homes)
            .unwrap_err()
            .to_string()
            .contains("aliases another home mount"));

        let duplicated_home_identity = format!(
            "{base}6 1 0:5 / /mnt/grant/own rw - btrfs own rw\n\
             7 4 0:5 / /home/other/own-alias rw - btrfs own rw\n"
        );
        assert!(check(&duplicated_home_identity)
            .unwrap_err()
            .to_string()
            .contains("aliases another home mount"));

        for admitted in [
            format!("{base}6 1 0:6 / /mnt/grant/nested rw - tmpfs nested rw\n"),
            format!("{base}6 1 8:1 /home/tester/Projects /mnt/grant/nested rw - ext4 root rw\n"),
            format!("{base}6 1 0:5 / /mnt/grant/nested rw - btrfs own rw\n"),
        ] {
            check(&admitted).unwrap();
        }
    }

    #[test]
    fn grant_mount_rows_are_unique_present_and_sorted_deepest_first() {
        let mountinfo = "1 0 0:1 / / rw - tmpfs root rw\n\
                         2 1 0:2 / /grant rw,nosuid,nodev,noexec - tmpfs grant rw\n\
                         3 2 0:3 / /grant/a rw,nosuid,nodev,noexec - tmpfs a rw\n\
                         4 2 0:4 / /grant/z rw,nosuid,nodev,noexec - tmpfs z rw\n\
                         5 3 0:5 / /grant/a/deep rw,nosuid,nodev,noexec - tmpfs deep rw\n";
        let mut rows = grant_mount_rows(mountinfo, Path::new("/grant")).unwrap();
        sort_grant_mount_rows(&mut rows);
        assert_eq!(
            rows.iter()
                .map(|row| row.mountpoint.as_path())
                .collect::<Vec<_>>(),
            [
                Path::new("/grant/a/deep"),
                Path::new("/grant/z"),
                Path::new("/grant/a"),
                Path::new("/grant"),
            ]
        );
        assert!(grant_mount_rows(
            &mountinfo.replace("/grant rw", "/other rw"),
            Path::new("/grant"),
        )
        .is_err());
        let duplicate =
            format!("{mountinfo}6 1 0:6 / /grant rw,nosuid,nodev,noexec - tmpfs duplicate rw\n");
        assert!(grant_mount_rows(&duplicate, Path::new("/grant")).is_err());
    }

    #[test]
    fn grant_mount_policy_flags_are_exact() {
        let hardened = sys::MS_REMOUNT
            | sys::MS_BIND
            | sys::MS_NOSUID
            | sys::MS_NODEV
            | sys::MS_NOEXEC;
        assert_eq!(grant_mount_policy_flags(false), hardened);
        assert_eq!(grant_mount_policy_flags(true), hardened | sys::MS_RDONLY);
    }

    #[test]
    fn grant_scaffolds_are_exhaustive_outside_private_home() {
        let base = grant_scaffold_names(true, &[]).unwrap();
        assert_eq!(
            base.get(Path::new("/home")),
            Some(&BTreeSet::from(["td".to_string()]))
        );
        let filesystems = [
            Stage2Filesystem {
                target: PathBuf::from("/mnt/media/pictures"),
                read_only: true,
                source_kind: FilesystemSourceKind::Directory,
            },
            Stage2Filesystem {
                target: PathBuf::from("/var/fixture-file"),
                read_only: true,
                source_kind: FilesystemSourceKind::File,
            },
            Stage2Filesystem {
                target: PathBuf::from("/home/td/Downloads"),
                read_only: false,
                source_kind: FilesystemSourceKind::Directory,
            },
            Stage2Filesystem {
                target: PathBuf::from("/home/tester/Projects"),
                read_only: false,
                source_kind: FilesystemSourceKind::Directory,
            },
        ];
        let names = grant_scaffold_names(true, &filesystems).unwrap();
        assert_eq!(
            names.get(Path::new("/mnt")),
            Some(&BTreeSet::from(["media".to_string()]))
        );
        assert_eq!(
            names.get(Path::new("/mnt/media")),
            Some(&BTreeSet::from(["pictures".to_string()]))
        );
        assert_eq!(
            names.get(Path::new("/var")),
            Some(&BTreeSet::from([
                "fixture-file".to_string(),
                "tmp".to_string(),
            ]))
        );
        assert_eq!(
            names.get(Path::new("/home")),
            Some(&BTreeSet::from(["td".to_string(), "tester".to_string()]))
        );
        assert!(!names.contains_key(Path::new("/home/td")));
        assert_eq!(
            names.get(Path::new("/home/tester")),
            Some(&BTreeSet::from(["Projects".to_string()]))
        );
    }

    #[test]
    fn writable_regular_file_probe_opens_without_changing_content() {
        let directory = temporary_directory().unwrap();
        let file = directory.join("grant-file");
        fs::write(&file, b"application-owned").unwrap();
        require_writable_file(&file).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"application-owned");
        fs::remove_file(&file).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn regular_file_identity_refuses_a_late_hardlink() {
        let directory = temporary_directory().unwrap();
        let file = directory.join("grant-file");
        fs::write(&file, b"application-owned").unwrap();
        let metadata = fs::symlink_metadata(&file).unwrap();
        let grant = FilesystemGrant {
            source: file.clone(),
            target: PathBuf::from("/grant-file"),
            read_only: true,
            source_kind: FilesystemSourceKind::File,
            source_device: metadata.dev(),
            source_inode: metadata.ino(),
        };
        require_filesystem_source_identity(&grant).unwrap();
        let alias = directory.join("grant-file-alias");
        fs::hard_link(&file, &alias).unwrap();
        assert!(require_filesystem_source_identity(&grant).is_err());
        fs::remove_file(alias).unwrap();
        fs::remove_file(file).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn filesystem_grant_readback_covers_every_nested_mount() {
        let read_only = Stage2Filesystem {
            target: PathBuf::from("/grant"),
            read_only: true,
            source_kind: FilesystemSourceKind::Directory,
        };
        let nested = "10 1 0:1 / / rw,nosuid,nodev,noexec - tmpfs tmpfs rw\n\
                      11 10 0:2 / /grant ro,nosuid,nodev,noexec - tmpfs tmpfs ro\n\
                      12 11 0:3 / /grant/nested ro,nosuid,nodev,noexec - tmpfs tmpfs ro\n";
        require_grant_mount_policy(nested, &read_only).unwrap();
        assert!(require_grant_mount_policy(
            &nested.replace(
                "/grant/nested ro,nosuid,nodev,noexec",
                "/grant/nested rw,nosuid,nodev,noexec"
            ),
            &read_only,
        )
        .is_err());
        assert!(require_grant_mount_policy(
            &nested.replace(
                "/grant/nested ro,nosuid,nodev,noexec",
                "/grant/nested ro,nosuid,nodev"
            ),
            &read_only,
        )
        .is_err());

        let read_write = Stage2Filesystem {
            target: PathBuf::from("/grant"),
            read_only: false,
            source_kind: FilesystemSourceKind::Directory,
        };
        let mixed = nested.replace(
            "/grant ro,nosuid,nodev,noexec",
            "/grant rw,nosuid,nodev,noexec",
        );
        require_grant_mount_policy(&mixed, &read_write).unwrap();
    }

    #[test]
    fn containment_readback_requires_the_parent_and_selected_scope() {
        let detached = "123 (td-jail helper) S 1 123 123 0 0 0 0\n";
        assert_eq!(process_containment(detached).unwrap().parent, 1);
        require_direct_parent(1, detached).unwrap();
        require_detached_session("helper", 123, detached).unwrap();
        require_supervised_group("helper", 123, detached).unwrap();
        assert!(require_direct_parent(2, detached).is_err());
        assert!(require_detached_session(
            "helper",
            123,
            "123 (td-jail helper) S 1 123 122 0 0 0 0\n"
        )
        .is_err());
        assert!(require_supervised_group(
            "helper",
            123,
            "123 (td-jail helper) S 1 122 122 0 0 0 0\n"
        )
        .is_err());
        assert!(require_detached_session(
            "helper",
            123,
            "123 (td-jail ) helper) S 1 123 123 34817 0 0 0\n"
        )
        .is_err());
    }
}

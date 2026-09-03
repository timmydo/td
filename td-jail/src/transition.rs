use crate::{
    authority::{
        self, decode_mountinfo_path, mount_identity_for_path, path_is_same_or_child,
        paths_overlap, FilesystemGrant, FilesystemSourceKind, LaunchPlan, ResolvedFile,
        ResolvedResourceLimits,
    },
    cgroup, firefox, seccomp, sys,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const PROBE_ARG: &str = "--probe-transition";
pub const RESOURCE_PROBE_ARG: &str = "--probe-resource-caps";
pub const PROCESS_TOKEN_PROBE_ARG: &str = "--probe-process-token";
pub const FIREFOX_SUPPORT_PROBE_ARG: &str = "--probe-firefox-support";
pub const FIREFOX_NETWORK_PROBE_ARG: &str = "--probe-firefox-network";
pub const FIREFOX_SOAK_PROBE_ARG: &str = "--probe-firefox-soak";
pub const FIREFOX_INPUT_PROBE_ARG: &str = "--probe-firefox-input";
pub const FIREFOX_DOWNLOAD_PROBE_ARG: &str = "--probe-firefox-download";
const FILTER_ARG: &str = "--internal-write-seccomp-filter";
const APPLICATION_SESSION_ARG: &str = "--internal-application-session";
const CGROUP_CLEANUP_ARG: &str = "--internal-cgroup-cleanup";
const CGROUP_CLEANUP_WATCH_ARG: &str = "--internal-cgroup-cleanup-watch";
const CGROUP_CLEANUP_READY: [u8; 1] = [1];
const STAGE2_ARG: &str = "--internal-stage-2";
const STAGE2_PROBE_ARG: &str = "--probe";
const STAGE2_LAUNCH_ARG: &str = "--launch";
const STAGE2_RESOLV_CONF_ARG: &str = "--resolv-conf";
const STAGE2_MACHINE_ID_ARG: &str = "--machine-id";
const STAGE2_TIMEZONE_ARG: &str = "--timezone";
const STAGE2_FIREFOX_AUTOTEST_POLICY_ARG: &str =
    "--firefox-autotest-policy";
const STAGE2_LOADER_LIBRARY_PATH_ARG: &str = "--loader-library-path";
const STAGE2_ENVIRONMENT_ARG: &str = "--environment";
const STAGE2_FILESYSTEMS_ARG: &str = "--filesystems";
const STAGE2_RESOURCES_ARG: &str = "--resources";
const STAGE2_ARGUMENTS_ARG: &str = "--arguments";
const NO_CGROUP_MEMBERSHIP: &str = "none";
/// Stage 2's spelling for "this launch carries no zone". A tzdb zone name
/// starts every component with a capital, so no zone can be spelled this.
const NO_TIMEZONE: &str = "absent";
const REAPER_CHILD_ARG: &str = "--internal-reaper-child";
const REAPER_ORPHAN_ARG: &str = "--internal-reaper-orphan";
const SURVIVOR_CHILD_ARG: &str = "--internal-survivor-child";
const SURVIVOR_ORPHAN_ARG: &str = "--internal-survivor-orphan";
/// §H item 12's three roles. The driver is the only one an operator names.
///
/// The driver spawns the stage-1 role rather than being stage 1 itself for
/// one reason that decides the whole shape: it has to KILL stage 1, and the
/// only kill safe Rust offers is `Child::kill` on a process you spawned.
/// Reaching a pid td-jail did not spawn would mean `kill(2)` with a real pid,
/// and `UNSAFE.md` §9 pins that syscall to pid -1 — so an outside-in probe
/// would have to widen the crate's audited syscall surface to test it.
pub const KILL_REAPS_PROBE_ARG: &str = "--probe-kill-reaps";
const KILL_REAPS_STAGE1_ARG: &str = "--internal-kill-reaps-stage-1";
const KILL_HOLD_CHILD_ARG: &str = "--internal-kill-hold-child";
const STAGE2_KILL_HOLD_ARG: &str = "--kill-hold";
pub const TRANSITION_MARKER: &str = "TD-JAIL-TRANSITION-OK";
pub const HOST_DEGRADATION_CGROUP: &str =
    "TD-JAIL-HOST-DEGRADATION aggregate-memory-task-and-cpu-caps=unenforced reason=no-delegated-cgroup";
pub const HOST_DEGRADATION_WAYLAND: &str =
    "TD-JAIL-HOST-DEGRADATION wayland-global-filter=unenforced reason=direct-host-socket";
const STAGE2_MARKER: &str = "TD-JAIL-STAGE2-OK";
/// What the §H item 12 driver prints once the killed instance is gone.
///
/// Fixed rather than carrying the pids it observed: the pids differ every
/// boot, and a console oracle that has to pattern-match cannot say the line
/// was produced by the code that did the proving. The pids go on a separate
/// diagnostic line that nothing depends on.
///
/// That diagnostic reaches the console only when the probe FAILS, which is
/// when it is worth reading: the boot leg captures the probe's whole output
/// in a command substitution and echoes it back only on the failure path.
/// On success the leg prints this marker and nothing else.
pub const KILL_REAPS_MARKER: &str = "TD-JAIL-KILL-REAPS-OK";
const STAGE2_HOLD_MARKER: &str = "TD-JAIL-STAGE2-HOLDING";
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
/// How long §H item 12's driver gives the kernel to empty the killed
/// instance out of the host's `/proc`.
///
/// Generous because the failure it must not produce is a flake: the teardown
/// it waits for is a kernel one with no userspace polling in it, so a real
/// pass takes milliseconds and only a real defect approaches this.
const KILL_REAPS_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_REAPS_POLL: Duration = Duration::from_millis(10);
/// The ceiling on every process that deliberately holds still for item 12.
///
/// The driver's own deadline is far shorter, so no passing run reaches this.
/// It exists so that a defect in the probe cannot leave a jailed process
/// holding a boot open until the QEMU oracle's whole-run timeout, which
/// would report the failure as something else entirely.
const KILL_HOLD_CEILING: Duration = Duration::from_secs(120);
/// The ceiling on the two roles that BLOCK, which is a different problem
/// from the ceiling on the two that hold.
///
/// `hold_until_reaped` only binds a role that has already reported. A stage
/// 2 that hangs BEFORE its report leaves stage 1 blocked in
/// `read_report_line`, and a stage 1 that hangs leaves the driver blocked in
/// the same call -- and neither read can carry a deadline of its own,
/// because a pipe read in `std` has no timeout. So the deadline lives
/// outside the read, in a thread.
///
/// Below `KILL_HOLD_CEILING` on purpose: the blocked roles give up first,
/// and the holders then expire on their own rather than being left behind
/// by a process that has already reported failure.
const KILL_REAPS_CEILING: Duration = Duration::from_secs(60);
const KILL_HOLD_POLL: Duration = Duration::from_millis(50);
/// Ceiling on the one line the stage-1 role reports to the driver, and on the
/// one the held stage 2 reports to it.
const KILL_REAPS_REPORT_LIMIT: u64 = 128;
/// Ceiling on the `/proc` walk that resolves the jailed descendant.
const MAX_PROC_SCAN: usize = 4096;
const SURVIVOR_TERM_TIMEOUT: Duration = Duration::from_secs(2);
const SURVIVOR_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const SURVIVOR_PROBE_LIFETIME: Duration = Duration::from_secs(30);
const PULSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STAGE2_OUTPUT_LIMIT: usize = 4096;
const WRITE_PROBE_PREFIX: &str = ".td-jail-write-probe-";
const ETC_SIZE_BYTES: usize = 8 * 1024 * 1024;
const NSSWITCH_CONF: &str = "passwd: files\ngroup: files\nhosts: files dns\n";
const RUNTIME_ROOT: &str = "/usr";
const LOCALTIME_PATH: &str = "/etc/localtime";
const TD_OWNED_ETC_NAMES: &[&str] = &[
    "group",
    "hostname",
    "hosts",
    "localtime",
    "machine-id",
    "nsswitch.conf",
    "passwd",
    "resolv.conf",
    "ssl",
];
const RUNTIME_ETC_ALLOWLIST: &[&str] = &[
    "dconf",
    "fonts",
    "gtk-3.0",
    "gtk-4.0",
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "pango",
    "pulse",
    "vulkan",
    "xdg",
];
const RUNTIME_ALIASES: &[(&str, &str)] = &[
    ("bin", "/usr/bin"),
    ("lib", "/usr/lib"),
    ("lib64", "/usr/lib64"),
    ("sbin", "/usr/sbin"),
];

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

    fn require_application_change(&self, before: &Self, isolate_network: bool) -> io::Result<()> {
        for (name, old, new) in [
            ("user", &before.user, &self.user),
            ("mount", &before.mount, &self.mount),
            ("uts", &before.uts, &self.uts),
        ] {
            if old == new {
                return Err(io::Error::other(format!(
                    "unshare reported success but the {name} namespace did not change"
                )));
            }
        }
        match (isolate_network, self.network == before.network) {
            (true, true) => Err(io::Error::other(
                "unshare reported success but the network namespace did not change",
            )),
            (false, false) => Err(io::Error::other(
                "shared network policy unexpectedly changed the network namespace",
            )),
            _ => Ok(()),
        }
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
    ProcessTokenProbe {
        application: String,
        token: String,
    },
    FirefoxSupportProbe,
    FirefoxNetworkProbe,
    FirefoxSoakProbe,
    FirefoxInputProbe {
        stage: firefox::InputStage,
    },
    FirefoxDownloadProbe,
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
    KillReapsProbe,
    KillReapsStage1 {
        expected_parent: u32,
    },
    KillHoldChild,
}

/// Everything stage 2 was told to launch, which is most of what `Mode` is.
///
/// Held BEHIND the variant rather than in it: inline, this one case decided
/// the size of every `Mode` value the parser returns, including the modes that
/// carry nothing. It is parsed once per process and consumed once, so the
/// indirection costs a pointer hop on a path that runs a single time.
#[derive(Debug, Eq, PartialEq)]
pub struct Stage2Launch {
    entry: String,
    resolv_conf: bool,
    machine_id: String,
    timezone: Option<String>,
    firefox_autotest_policy: bool,
    runtime_aliases: bool,
    environment: Vec<(OsString, OsString)>,
    filesystems: Vec<Stage2Filesystem>,
    resources: ResolvedResourceLimits,
    cgroup_membership: String,
    arguments: Vec<OsString>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Stage2Action {
    Probe,
    /// §H item 12: become the instance, then wait to be torn down.
    ///
    /// It shares `Probe`'s containment exactly — same absent filesystem plan,
    /// same empty `/etc` binding, same probe-only reaper-probe bind — because
    /// the thing under test is what stage 1's death does, not what stage 2
    /// was told to mount. The one difference is the terminal arm.
    KillHold,
    Launch(Box<Stage2Launch>),
}

#[derive(Clone, Copy)]
struct Stage2ResourceBinding<'a> {
    limits: ResolvedResourceLimits,
    membership: &'a str,
}

/// What this launch's selective `/etc` CONTAINS, carried as one value.
///
/// Four facts rather than four parameters because they are one decision, and
/// because two adjacent booleans threaded through two frames are two chances
/// to transpose them — which would build one `/etc` and prove a different one
/// correct, with both halves agreeing.
#[derive(Clone, Copy)]
struct EtcBinding<'a> {
    resolv_conf: bool,
    timezone: Option<&'a str>,
    firefox_autotest_policy: bool,
    machine_id: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct Stage2MountBinding<'a> {
    filesystems: &'a [FilesystemGrant],
    resolv_conf: bool,
    machine_id: &'a str,
    timezone: Option<&'a str>,
    firefox_autotest_policy: bool,
    loader_library_path: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeEtcEntry {
    name: &'static str,
    source: PathBuf,
    source_kind: FilesystemSourceKind,
    source_device: u64,
    source_inode: u64,
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
    if mode == PROCESS_TOKEN_PROBE_ARG {
        let application = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        let token = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        authority::validate_application_name(&application)?;
        if !cgroup::valid_process_token(&token) {
            return Err(usage_error());
        }
        return Ok(Mode::ProcessTokenProbe { application, token });
    }
    if mode == FIREFOX_SUPPORT_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::FirefoxSupportProbe);
    }
    if mode == FIREFOX_NETWORK_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::FirefoxNetworkProbe);
    }
    if mode == FIREFOX_SOAK_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::FirefoxSoakProbe);
    }
    if mode == FIREFOX_INPUT_PROBE_ARG {
        let stage = args
            .next()
            .and_then(|value| value.into_string().ok())
            .and_then(|value| firefox::InputStage::parse(&value))
            .ok_or_else(usage_error)?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::FirefoxInputProbe { stage });
    }
    if mode == FIREFOX_DOWNLOAD_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::FirefoxDownloadProbe);
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
    if mode == KILL_REAPS_PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::KillReapsProbe);
    }
    if mode == KILL_REAPS_STAGE1_ARG {
        let expected_parent = parse_positive_pid(args.next())?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::KillReapsStage1 { expected_parent });
    }
    if mode == KILL_HOLD_CHILD_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::KillHoldChild);
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
    if action == STAGE2_KILL_HOLD_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Stage2Action::KillHold);
    }
    if action == STAGE2_LAUNCH_ARG {
        let entry = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        authority::validate_entry(&entry)?;
        if args.next().as_deref() != Some(STAGE2_RESOLV_CONF_ARG.as_ref()) {
            return Err(usage_error());
        }
        let resolv_conf = match args.next().as_deref().and_then(OsStr::to_str) {
            Some("present") => true,
            Some("absent") => false,
            _ => return Err(usage_error()),
        };
        if args.next().as_deref() != Some(STAGE2_MACHINE_ID_ARG.as_ref()) {
            return Err(usage_error());
        }
        let machine_id = args
            .next()
            .ok_or_else(usage_error)?
            .into_string()
            .map_err(|_| usage_error())?;
        if !authority::valid_machine_id(&machine_id) {
            return Err(usage_error());
        }
        if args.next().as_deref() != Some(STAGE2_TIMEZONE_ARG.as_ref()) {
            return Err(usage_error());
        }
        let timezone = match args.next().ok_or_else(usage_error)?.into_string() {
            Ok(value) if value == NO_TIMEZONE => None,
            Ok(value) if authority::valid_timezone_name(&value) => Some(value),
            _ => return Err(usage_error()),
        };
        if args.next().as_deref()
            != Some(STAGE2_FIREFOX_AUTOTEST_POLICY_ARG.as_ref())
        {
            return Err(usage_error());
        }
        let firefox_autotest_policy =
            match args.next().as_deref().and_then(OsStr::to_str) {
                Some("present") => true,
                Some("absent") => false,
                _ => return Err(usage_error()),
            };
        if args.next().as_deref() != Some(STAGE2_LOADER_LIBRARY_PATH_ARG.as_ref()) {
            return Err(usage_error());
        }
        let loader_library_path = match args.next().as_deref().and_then(OsStr::to_str) {
            Some("absent") => None,
            Some("present") => Some(
                args.next()
                    .ok_or_else(usage_error)?
                    .into_string()
                    .map_err(|_| usage_error())?,
            ),
            _ => return Err(usage_error()),
        };
        authority::validate_stage2_loader_library_path(loader_library_path.as_deref())?;
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
        authority::validate_environment_list(
            &environment,
            uid,
            loader_library_path.as_deref(),
            // Stage 1 emitted this authenticated environment from the
            // permission-bearing ApplicationSpec. Here the environment is
            // the stage-2 wire format, so this proves only that its Pulse
            // pair remains internally complete and exact.
            environment.iter().any(|(key, _)| key == "PULSE_SERVER"),
        )?;
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
        let cpu_quota_usec = parse_u64(args.next(), "cpu-max quota")?;
        let cpu_period_usec = parse_u64(args.next(), "cpu-max period")?;
        let resources = ResolvedResourceLimits::from_stage2(
            memory_high_bytes,
            memory_max_bytes,
            pids_max,
            cpu_quota_usec,
            cpu_period_usec,
        )?;
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
        return Ok(Stage2Action::Launch(Box::new(Stage2Launch {
            entry,
            resolv_conf,
            machine_id,
            timezone,
            firefox_autotest_policy,
            runtime_aliases: loader_library_path.is_some(),
            environment,
            filesystems,
            resources,
            cgroup_membership,
            arguments: authority::collect_arguments(args)?,
        })));
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
    mounts: Stage2MountBinding<'_>,
    resources: Stage2ResourceBinding<'_>,
    arguments: &[OsString],
) -> Vec<OsString> {
    let mut stage2 = Vec::with_capacity(
        24usize
            .saturating_add(environment.len().saturating_mul(2))
            .saturating_add(mounts.filesystems.len().saturating_mul(2))
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
        OsString::from(STAGE2_RESOLV_CONF_ARG),
        OsString::from(if mounts.resolv_conf {
            "present"
        } else {
            "absent"
        }),
        OsString::from(STAGE2_MACHINE_ID_ARG),
        OsString::from(mounts.machine_id),
        OsString::from(STAGE2_TIMEZONE_ARG),
        OsString::from(mounts.timezone.unwrap_or(NO_TIMEZONE)),
        OsString::from(STAGE2_FIREFOX_AUTOTEST_POLICY_ARG),
        OsString::from(if mounts.firefox_autotest_policy {
            "present"
        } else {
            "absent"
        }),
    ]);
    stage2.push(OsString::from(STAGE2_LOADER_LIBRARY_PATH_ARG));
    match mounts.loader_library_path {
        Some(value) => {
            stage2.push(OsString::from("present"));
            stage2.push(OsString::from(value));
        }
        None => stage2.push(OsString::from("absent")),
    }
    stage2.extend([
        OsString::from(STAGE2_ENVIRONMENT_ARG),
        OsString::from(environment.len().to_string()),
    ]);
    stage2.extend(
        environment
            .iter()
            .flat_map(|(key, value)| [key.clone(), value.clone()]),
    );
    stage2.push(OsString::from(STAGE2_FILESYSTEMS_ARG));
    stage2.push(OsString::from(mounts.filesystems.len().to_string()));
    for filesystem in mounts.filesystems {
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
        OsString::from(resources.limits.cpu_quota_usec.to_string()),
        OsString::from(resources.limits.cpu_period_usec.to_string()),
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
        "bare td-jail accepts only --probe-transition, --probe-kill-reaps, --probe-resource-caps NAME, --probe-process-token NAME TOKEN, --probe-firefox-support, --probe-firefox-network, --probe-firefox-soak, --probe-firefox-download, or --probe-firefox-input arm|menu|final|clipboard-refocus-arm|clipboard-refocus|clipboard|download|file-chooser|file-chooser-focus|file-chooser-result; installed applications are selected by argv[0]",
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

fn mount_bind_kind(
    source: &Path,
    target: &Path,
    source_kind: FilesystemSourceKind,
    label: &str,
) -> io::Result<()> {
    let source_text = source.to_str().ok_or_else(|| {
        io::Error::other(format!("{label} source is not UTF-8: {}", source.display()))
    })?;
    let target_text = target.to_str().ok_or_else(|| {
        io::Error::other(format!("{label} target is not UTF-8: {}", target.display()))
    })?;
    let flags = sys::MS_BIND
        | if source_kind == FilesystemSourceKind::Directory {
            sys::MS_REC
        } else {
            0
        };
    sys::mount(
        Some(&cstring(source_text)?),
        &cstring(target_text)?,
        None,
        flags,
        None,
    )
    .map_err(|error| {
        io::Error::other(format!(
            "bind {label} {} at {}: {error}",
            source.display(),
            target.display()
        ))
    })
}

fn mount_filesystem_grant(grant: &FilesystemGrant) -> io::Result<()> {
    authority::validate_filesystem_target(&grant.target)?;
    require_filesystem_source_identity(grant)?;
    let target = prepare_filesystem_target(&grant.target, grant.source_kind)?;
    mount_bind_kind(
        &grant.source,
        &target,
        grant.source_kind,
        "filesystem grant",
    )?;
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

fn selected_runtime_etc(runtime_files: &Path) -> io::Result<Vec<RuntimeEtcEntry>> {
    validate_runtime_etc_allowlist()?;
    let root = runtime_files.join("etc");
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::other(format!(
                "runtime configuration root {} is not a direct directory",
                root.display()
            )));
        }
    }

    let mut selected = Vec::new();
    for name in RUNTIME_ETC_ALLOWLIST {
        let source = root.join(name);
        let metadata = match fs::symlink_metadata(&source) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(metadata) => metadata,
        };
        let source_kind = if metadata.file_type().is_dir() {
            FilesystemSourceKind::Directory
        } else if metadata.file_type().is_file() {
            FilesystemSourceKind::File
        } else {
            return Err(io::Error::other(format!(
                "runtime configuration {} is not a direct file or directory",
                source.display()
            )));
        };
        selected.push(RuntimeEtcEntry {
            name,
            source,
            source_kind,
            source_device: metadata.dev(),
            source_inode: metadata.ino(),
        });
    }
    Ok(selected)
}

fn validate_runtime_etc_allowlist() -> io::Result<()> {
    let mut previous = None;
    for name in RUNTIME_ETC_ALLOWLIST {
        if name.is_empty()
            || name.contains('/')
            || previous.is_some_and(|prior: &str| prior >= *name)
            || TD_OWNED_ETC_NAMES.contains(name)
        {
            return Err(io::Error::other(
                "runtime configuration allowlist is not sorted, unique and disjoint",
            ));
        }
        previous = Some(name);
    }
    Ok(())
}

fn require_runtime_etc_source_identity(entry: &RuntimeEtcEntry) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&entry.source)?;
    let kind_matches = match entry.source_kind {
        FilesystemSourceKind::Directory => metadata.file_type().is_dir(),
        FilesystemSourceKind::File => metadata.file_type().is_file(),
    };
    if !kind_matches
        || metadata.dev() != entry.source_device
        || metadata.ino() != entry.source_inode
    {
        return Err(io::Error::other(format!(
            "runtime configuration {} changed after selection",
            entry.source.display()
        )));
    }
    Ok(())
}

fn create_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("create jail file {}: {error}", path.display()),
            )
        })?;
    file.write_all(contents)?;
    drop(file);
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn read_bounded_text(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut text = String::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)?;
    if u64::try_from(text.len()).map_or(true, |length| length > max_bytes) {
        return Err(io::Error::other(format!(
            "text file {} exceeds {max_bytes} bytes",
            path.display()
        )));
    }
    Ok(text)
}

fn inherited_hostname() -> io::Result<String> {
    let text = read_bounded_text(Path::new("/proc/sys/kernel/hostname"), 65)?;
    let hostname = text.strip_suffix('\n').unwrap_or(&text);
    if hostname.is_empty()
        || hostname.len() > 64
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(io::Error::other(
            "inherited UTS hostname is not a canonical ASCII hostname",
        ));
    }
    Ok(hostname.to_string())
}

fn passwd(identity: Identity) -> String {
    format!(
        "root:x:0:0:root:/root:/usr/bin/false\n\
td:x:{}:{}:td:/home/td:/usr/bin/false\n\
nobody:x:65534:65534:nobody:/:/usr/bin/false\n",
        identity.uid, identity.gid
    )
}

fn group(identity: Identity) -> String {
    format!(
        "root:x:0:\n\
td:x:{}:\n\
nobody:x:65534:\n",
        identity.gid
    )
}

fn hosts(hostname: &str) -> String {
    format!(
        "127.0.0.1 localhost {hostname}\n::1 localhost ip6-localhost ip6-loopback {hostname}\n"
    )
}

fn require_resolved_file_identity(file: &ResolvedFile, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&file.path)?;
    if !metadata.file_type().is_file()
        || metadata.dev() != file.device
        || metadata.ino() != file.inode
        || metadata.len() != file.size
    {
        return Err(io::Error::other(format!(
            "{label} {} changed after authority resolution",
            file.path.display()
        )));
    }
    Ok(())
}

fn mount_runtime_etc_entry(entry: &RuntimeEtcEntry, etc: &Path) -> io::Result<()> {
    require_runtime_etc_source_identity(entry)?;
    let target = etc.join(entry.name);
    match entry.source_kind {
        FilesystemSourceKind::Directory => {
            create_dir(
                target
                    .to_str()
                    .ok_or_else(|| io::Error::other("runtime etc target is not UTF-8"))?,
                0o555,
            )?;
        }
        FilesystemSourceKind::File => create_file(&target, b"", 0o444)?,
    }
    mount_bind_kind(
        &entry.source,
        &target,
        entry.source_kind,
        "runtime configuration",
    )?;
    apply_grant_mount_policy(&target, true)?;
    let target_metadata = fs::symlink_metadata(&target)?;
    if entry.source_device != target_metadata.dev()
        || entry.source_inode != target_metadata.ino()
        || (target_metadata.file_type().is_dir()
            != (entry.source_kind == FilesystemSourceKind::Directory))
        || (target_metadata.file_type().is_file()
            != (entry.source_kind == FilesystemSourceKind::File))
    {
        return Err(io::Error::other(format!(
            "runtime configuration bind {} changed identity",
            entry.name
        )));
    }
    require_runtime_etc_source_identity(entry)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_bind_source(&mountinfo, &entry.source, &target)?;
    require_grant_mount_policy(
        &mountinfo,
        &Stage2Filesystem {
            target,
            read_only: true,
            source_kind: entry.source_kind,
        },
    )
}

fn mount_resolved_file(file: &ResolvedFile, target: &Path, label: &str) -> io::Result<()> {
    require_resolved_file_identity(file, label)?;
    create_file(target, b"", 0o444)?;
    mount_private_bind(
        &file.path,
        target
            .to_str()
            .ok_or_else(|| io::Error::other("resolved file target is not UTF-8"))?,
        true,
    )?;
    let metadata = fs::symlink_metadata(target)?;
    if !metadata.file_type().is_file()
        || metadata.dev() != file.device
        || metadata.ino() != file.inode
        || metadata.len() != file.size
    {
        return Err(io::Error::other(format!(
            "{label} bind {} changed identity",
            target.display()
        )));
    }
    require_resolved_file_identity(file, label)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    require_bind_source(&mountinfo, &file.path, target)?;
    require_mount(
        &mountinfo,
        target
            .to_str()
            .ok_or_else(|| io::Error::other("resolved file target is not UTF-8"))?,
        None,
        &["ro", "nosuid", "nodev", "noexec"],
        &["rw"],
    )
}

fn prepare_etc(application: &LaunchPlan) -> io::Result<()> {
    let etc = PathBuf::from(format!("{NEW_ROOT}/etc"));
    let identity = Identity {
        uid: application.inside_uid,
        gid: application.inside_gid,
    };
    let etc_text = etc
        .to_str()
        .ok_or_else(|| io::Error::other("jail etc path is not UTF-8"))?;
    mount_tmpfs(
        etc_text,
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
        &format!("mode=0755,size={ETC_SIZE_BYTES}"),
    )?;
    let hostname = inherited_hostname()?;
    for (name, contents) in [
        ("passwd", passwd(identity)),
        ("group", group(identity)),
        ("nsswitch.conf", NSSWITCH_CONF.to_string()),
        ("machine-id", application.state.machine_id.clone()),
        ("hostname", format!("{hostname}\n")),
        ("hosts", hosts(&hostname)),
    ] {
        create_file(&etc.join(name), contents.as_bytes(), 0o444)?;
    }
    create_dir(&format!("{etc_text}/ssl"), 0o555)?;
    create_dir(&format!("{etc_text}/ssl/certs"), 0o555)?;
    mount_resolved_file(
        &application.ca_bundle,
        &etc.join("ssl/certs/ca-certificates.crt"),
        "application CA bundle",
    )?;
    if let Some(resolver) = &application.resolv_conf {
        mount_resolved_file(
            resolver,
            &etc.join("resolv.conf"),
            "application resolver configuration",
        )?;
    }
    // glibc reads `/etc/localtime` when `TZ` is unset, so the bind IS the
    // mechanism: no environment entry, no per-application spec row, and
    // nothing an application has to be told. Absent when the launch carries no
    // zone, which leaves glibc on its own built-in UTC.
    if let Some(timezone) = &application.timezone {
        mount_resolved_file(
            &timezone.file,
            &etc.join("localtime"),
            "application timezone",
        )?;
    }
    if let Some(firefox) = &application.firefox_autotest_policy {
        create_dir(&format!("{etc_text}/firefox"), 0o555)?;
        create_dir(&format!("{etc_text}/firefox/policies"), 0o555)?;
        mount_resolved_file(
            &firefox.policy,
            &etc.join("firefox/policies/policies.json"),
            "Firefox autotest policy",
        )?;
        mount_resolved_file(
            &firefox.ca,
            &etc.join("firefox/policies/td-firefox-autotest-ca.pem"),
            "Firefox autotest CA",
        )?;
    }
    for entry in selected_runtime_etc(&application.runtime_files)? {
        mount_runtime_etc_entry(&entry, &etc)?;
    }
    remount_read_only(
        etc_text,
        sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC,
    )
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
            (format!("{NEW_ROOT}/etc"), 0o755),
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
        let pulse = pulse_mount_plan(application.pulse_runtime.as_deref());
        if let Some(pulse) = &pulse {
            create_dir(&format!("{run}/flatpak"), 0o755)?;
            create_dir(&format!("{run}/flatpak/pulse"), 0o755)?;
            create_dir(&pulse.runtime_target, 0o755)?;
            symlink("server/native", format!("{run}/flatpak/pulse/native"))?;
            fs::write(
                &pulse.config_target,
                crate::permissions::APPLICATION_PULSE_CONFIG,
            )?;
            fs::set_permissions(
                &pulse.config_target,
                fs::Permissions::from_mode(0o444),
            )?;
            // The tmpfs is made by the mapped application identity, so 0444
            // alone is not immutable. Make the highest private ancestor a
            // read-only mountpoint before adding the server child mount. That
            // prevents replacing either `flatpak` or `pulse`; binding after
            // the child mount would need a recursive bind to preserve it.
            let flatpak_directory = format!("{run}/flatpak");
            mount_private_bind(Path::new(&flatpak_directory), &flatpak_directory, true)?;
        }

        create_dir(&format!("{NEW_ROOT}/home/td"), 0o700)?;
        mount_application_tree(&application.package_files, &format!("{NEW_ROOT}/app"))?;
        mount_application_tree(&application.runtime_files, &format!("{NEW_ROOT}/usr"))?;
        if application.loader_library_path.is_some() {
            install_runtime_aliases()?;
        }
        prepare_etc(application)?;
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
        if let Some(pulse) = &pulse {
            mount_private_bind(pulse.source, &pulse.runtime_target, true)?;
        }
        for filesystem in &application.filesystems {
            mount_filesystem_grant(filesystem)?;
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        let mut mounted = vec![
            (
                application.package_files.clone(),
                format!("{NEW_ROOT}/app"),
            ),
            (
                application.runtime_files.clone(),
                format!("{NEW_ROOT}/usr"),
            ),
            (
                application.state.home.clone(),
                format!("{NEW_ROOT}/home/td"),
            ),
            (
                application.state.config.clone(),
                format!("{NEW_ROOT}/home/td/.config"),
            ),
            (
                application.state.cache.clone(),
                format!("{NEW_ROOT}/home/td/.cache"),
            ),
            (
                application.state.data.clone(),
                format!("{NEW_ROOT}/home/td/.local/share"),
            ),
            (
                application.state.local_state.clone(),
                format!("{NEW_ROOT}/home/td/.local/state"),
            ),
            (application.state.runtime.clone(), application_runtime),
            (application.wayland_socket.clone(), wayland),
            (application.bus_socket.clone(), bus),
        ];
        if let Some(pulse) = pulse {
            mounted.push((pulse.source.to_path_buf(), pulse.runtime_target));
            let path = format!("{NEW_ROOT}/run/flatpak");
            mounted.push((PathBuf::from(&path), path));
        }
        for (source, target) in mounted {
            require_bind_source(&mountinfo, &source, Path::new(&target))?;
        }
    }
    if application.is_none() {
        mount_reaper_probe(executable)?;
    }
    remount_read_only(&dev, sys::MS_NOSUID | sys::MS_NOEXEC)
}

#[derive(Debug, Eq, PartialEq)]
struct PulseMountPlan<'a> {
    source: &'a Path,
    runtime_target: String,
    config_target: String,
}

fn pulse_mount_plan(source: Option<&Path>) -> Option<PulseMountPlan<'_>> {
    source.map(|source| PulseMountPlan {
        source,
        runtime_target: format!("{NEW_ROOT}/run/flatpak/pulse/server"),
        config_target: format!(
            "{NEW_ROOT}{}",
            crate::permissions::APPLICATION_PULSE_CONFIG_PATH
        ),
    })
}

fn pulse_socket_mode(pulse: bool, host_mode: bool) -> Option<u32> {
    (pulse && !host_mode).then_some(crate::permissions::TD_AUDIO_SOCKET_MODE)
}

fn install_runtime_aliases() -> io::Result<()> {
    for (name, target) in RUNTIME_ALIASES {
        symlink(target, format!("{NEW_ROOT}/{name}"))?;
    }
    Ok(())
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

fn required_run_names(pulse: bool) -> &'static [&'static str] {
    if pulse {
        &["flatpak", "user"]
    } else {
        &["user"]
    }
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
    runtime_aliases: bool,
    filesystems: &[Stage2Filesystem],
) -> io::Result<BTreeMap<PathBuf, BTreeSet<String>>> {
    let mut root = if application {
        ["app", "dev", "etc", "home", "proc", "run", "tmp", "usr", "var"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    } else {
        ["dev", "proc", "tmp", "var"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    };
    if runtime_aliases {
        root.extend(
            RUNTIME_ALIASES
                .iter()
                .map(|(name, _)| (*name).to_string()),
        );
    }
    let mut expected = BTreeMap::from([
        (PathBuf::from("/"), root),
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

/// Takes the binding whole rather than the three booleans inside it: adjacent
/// same-typed arguments at a CALL SITE are the transposition no callee-side
/// test can see.
fn expected_etc_names(
    runtime_etc: &[RuntimeEtcEntry],
    etc: EtcBinding<'_>,
) -> io::Result<BTreeSet<String>> {
    validate_runtime_etc_allowlist()?;
    let mut expected = TD_OWNED_ETC_NAMES
        .iter()
        .filter(|name| etc.resolv_conf || **name != "resolv.conf")
        .filter(|name| etc.timezone.is_some() || **name != "localtime")
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    if etc.firefox_autotest_policy && !expected.insert("firefox".to_string()) {
        return Err(io::Error::other(
            "Firefox autotest policy collides with selective /etc",
        ));
    }
    for entry in runtime_etc {
        if !RUNTIME_ETC_ALLOWLIST.contains(&entry.name) || !expected.insert(entry.name.to_string())
        {
            return Err(io::Error::other(
                "runtime configuration selection is not closed and unique",
            ));
        }
    }
    Ok(expected)
}

/// `/etc/localtime` is the runtime's OWN file for the zone this system names,
/// and not merely some TZif.
///
/// Identity is what makes it THE zone rather than A zone: two zones are two
/// files, so a bind of the wrong one has the wrong inode and this notices. The
/// source is read back through `/usr`, the same runtime tree stage 1 resolved
/// the zone in and `require_runtime_etc_source_identity` already pins, so the
/// question asked inside is the question answered outside.
///
/// The magic is checked again here, on the file the application will actually
/// open, because glibc reads a non-TZif `/etc/localtime` as UTC in silence:
/// the failure it guards has no symptom other than a wrong clock.
fn require_bound_zone(zone: &str) -> io::Result<()> {
    require_bound_zone_at(Path::new(LOCALTIME_PATH), Path::new(RUNTIME_ROOT), zone)
}

/// The readback's rules, over paths a test can build.
///
/// Split out because the shipped caller only ever runs inside a jail that has
/// already pivoted, so nothing outside a booted image can execute it — and a
/// readback nothing has executed is a readback whose own bugs are invisible.
/// A hard link stands in for the bind here: a bind of a regular file and a
/// second link to it give the same `stat` and the same bytes, which is all
/// this function looks at.
///
/// It takes the RUNTIME root rather than the zoneinfo root so that the pair
/// of containment rules is the same pair stage 1 applies — canonical zoneinfo
/// inside the runtime, canonical zone inside that zoneinfo. Checking the zone
/// against an uncanonicalized `/usr/share/zoneinfo` instead would be a
/// DIFFERENT rule that neither implies nor is implied by stage 1's, and a
/// runtime whose `share` is a link to a sibling inside itself would pass
/// outside and abort a fully-built jail here.
fn require_bound_zone_at(
    localtime: &Path,
    runtime_root: &Path,
    zone: &str,
) -> io::Result<()> {
    if !authority::valid_timezone_name(zone) {
        return Err(io::Error::other(
            "stage-2 zone name is outside the compiled grammar",
        ));
    }
    let bound = fs::symlink_metadata(localtime)?;
    if !bound.file_type().is_file() {
        return Err(io::Error::other(
            "selective /etc localtime is not a regular file",
        ));
    }
    // Stage 1's pair, asked again of the same tree: the zoneinfo root stays
    // inside the runtime, and the zone stays inside that root. A rule proved
    // on one side of a bind only is a rule this readback is trusting stage 1
    // for rather than checking.
    let runtime = fs::canonicalize(runtime_root).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("resolve runtime root {}: {error}", runtime_root.display()),
        )
    })?;
    let zoneinfo = runtime.join(authority::ZONEINFO_SUBDIR);
    let root = fs::canonicalize(&zoneinfo).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("resolve runtime zoneinfo {}: {error}", zoneinfo.display()),
        )
    })?;
    if !root.starts_with(&runtime) {
        return Err(io::Error::other(
            "runtime zoneinfo resolves outside the runtime",
        ));
    }
    let source = root.join(zone);
    // Follows links, as the resolution outside did: a zone name is routinely
    // a symlink to another zone in the same tree.
    let canonical = fs::canonicalize(&source).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("resolve runtime zone {}: {error}", source.display()),
        )
    })?;
    if !canonical.starts_with(&root) {
        return Err(io::Error::other(
            "runtime zone resolves outside the runtime zoneinfo",
        ));
    }
    let expected = fs::symlink_metadata(&canonical).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect runtime zone {}: {error}", canonical.display()),
        )
    })?;
    if bound.dev() != expected.dev() || bound.ino() != expected.ino() {
        return Err(io::Error::other(
            "selective /etc localtime is not the runtime's own file for the system zone",
        ));
    }
    let mut magic = [0u8; authority::TZIF_MAGIC.len()];
    fs::File::open(localtime)?.read_exact(&mut magic)?;
    if &magic != authority::TZIF_MAGIC {
        return Err(io::Error::other(
            "selective /etc localtime is not a compiled TZif zone",
        ));
    }
    Ok(())
}

fn require_etc_plan(
    mountinfo: &str,
    token: &[u8; TOKEN_LEN],
    etc: EtcBinding<'_>,
    identity: Identity,
) -> io::Result<()> {
    let EtcBinding {
        resolv_conf,
        timezone,
        firefox_autotest_policy,
        machine_id,
    } = etc;
    let expected_machine_id = machine_id
        .ok_or_else(|| io::Error::other("application mount plan has no machine id"))?;
    let runtime_etc = selected_runtime_etc(Path::new("/usr"))?;
    let expected = expected_etc_names(&runtime_etc, etc)?;
    let actual = read_dir_names("/etc")?;
    if actual != expected {
        return Err(io::Error::other(format!(
            "selective /etc entries are not exact: {actual:?}"
        )));
    }
    require_names("/etc/ssl", &["certs"])?;
    require_names("/etc/ssl/certs", &["ca-certificates.crt"])?;
    require_mode("/etc", 0o755)?;
    require_mode("/etc/ssl", 0o555)?;
    require_mode("/etc/ssl/certs", 0o555)?;
    if firefox_autotest_policy {
        require_names("/etc/firefox", &["policies"])?;
        require_names(
            "/etc/firefox/policies",
            &["policies.json", "td-firefox-autotest-ca.pem"],
        )?;
        require_mode("/etc/firefox", 0o555)?;
        require_mode("/etc/firefox/policies", 0o555)?;
        require_mode("/etc/firefox/policies/policies.json", 0o444)?;
        require_mode(
            "/etc/firefox/policies/td-firefox-autotest-ca.pem",
            0o444,
        )?;
        if read_bounded_text(
            Path::new("/etc/firefox/policies/policies.json"),
            1024,
        )? != authority::FIREFOX_AUTOTEST_POLICY
        {
            return Err(io::Error::other(
                "Firefox autotest policy is not the compiled certificate policy",
            ));
        }
        let firefox_ca = read_bounded_text(
            Path::new("/etc/firefox/policies/td-firefox-autotest-ca.pem"),
            64 * 1024,
        )?;
        if !firefox_ca.starts_with("-----BEGIN CERTIFICATE-----\n")
            || !firefox_ca.ends_with("-----END CERTIFICATE-----\n")
            || firefox_ca.matches("-----BEGIN CERTIFICATE-----").count() != 1
        {
            return Err(io::Error::other(
                "Firefox autotest CA is not one bounded PEM certificate",
            ));
        }
    }
    for path in [
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/machine-id",
        "/etc/hostname",
        "/etc/hosts",
    ] {
        require_mode(path, 0o444)?;
    }
    if read_bounded_text(Path::new("/etc/passwd"), 4096)? != passwd(identity)
        || read_bounded_text(Path::new("/etc/group"), 4096)? != group(identity)
        || read_bounded_text(Path::new("/etc/nsswitch.conf"), 4096)? != NSSWITCH_CONF
    {
        return Err(io::Error::other(
            "selective /etc identity databases are not canonical",
        ));
    }
    let hostname = inherited_hostname()?;
    if read_bounded_text(Path::new("/etc/hostname"), 65)? != format!("{hostname}\n")
        || read_bounded_text(Path::new("/etc/hosts"), 4096)? != hosts(&hostname)
    {
        return Err(io::Error::other(
            "selective /etc hostname files do not match the UTS namespace",
        ));
    }
    let machine_id = read_bounded_text(Path::new("/etc/machine-id"), 33)?;
    if !authority::valid_machine_id(&machine_id) || machine_id != expected_machine_id {
        return Err(io::Error::other(
            "selective /etc machine-id does not match application state",
        ));
    }
    let ca_bundle = read_bounded_text(
        Path::new("/etc/ssl/certs/ca-certificates.crt"),
        authority::MAX_CA_BUNDLE_BYTES,
    )?;
    if !ca_bundle.contains("-----BEGIN CERTIFICATE-----") {
        return Err(io::Error::other(
            "selective /etc CA bundle has no PEM certificate",
        ));
    }

    require_mount(
        mountinfo,
        "/etc",
        Some("tmpfs"),
        &["ro", "nosuid", "nodev", "noexec"],
        &["rw"],
    )?;
    require_mount_super_option(mountinfo, "/etc", "size=8192k")?;
    require_mount(
        mountinfo,
        "/etc/ssl/certs/ca-certificates.crt",
        None,
        &["ro", "nosuid", "nodev", "noexec"],
        &["rw"],
    )?;
    if firefox_autotest_policy {
        for path in [
            "/etc/firefox/policies/policies.json",
            "/etc/firefox/policies/td-firefox-autotest-ca.pem",
        ] {
            require_mount(
                mountinfo,
                path,
                None,
                &["ro", "nosuid", "nodev", "noexec"],
                &["rw"],
            )?;
        }
    }
    if resolv_conf {
        if read_bounded_text(
            Path::new("/etc/resolv.conf"),
            authority::MAX_RESOLV_CONF_BYTES,
        )?
        .is_empty()
        {
            return Err(io::Error::other(
                "selective /etc resolver configuration is empty",
            ));
        }
        require_mount(
            mountinfo,
            "/etc/resolv.conf",
            None,
            &["ro", "nosuid", "nodev", "noexec"],
            &["rw"],
        )?;
    }
    if let Some(zone) = timezone {
        require_bound_zone(zone)?;
        require_mount(
            mountinfo,
            "/etc/localtime",
            None,
            &["ro", "nosuid", "nodev", "noexec"],
            &["rw"],
        )?;
    }
    for entry in runtime_etc {
        require_runtime_etc_source_identity(&entry)?;
        let target = PathBuf::from("/etc").join(entry.name);
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.dev() != entry.source_device
            || metadata.ino() != entry.source_inode
            || (metadata.file_type().is_dir()
                != (entry.source_kind == FilesystemSourceKind::Directory))
            || (metadata.file_type().is_file()
                != (entry.source_kind == FilesystemSourceKind::File))
        {
            return Err(io::Error::other(format!(
                "runtime configuration {} does not retain its source identity",
                target.display()
            )));
        }
        require_bind_source(mountinfo, &entry.source, &target)?;
        require_grant_mount_policy(
            mountinfo,
            &Stage2Filesystem {
                target,
                read_only: true,
                source_kind: entry.source_kind,
            },
        )?;
    }
    require_read_only_mount("/etc", token)
}

struct Stage2MountExpectation<'a> {
    filesystems: Option<&'a [Stage2Filesystem]>,
    etc: EtcBinding<'a>,
    runtime_aliases: bool,
    pulse: bool,
    pulse_socket_mode: Option<u32>,
}

fn require_mount_plan(
    expected: Stage2MountExpectation<'_>,
    token: &[u8; TOKEN_LEN],
    identity: Identity,
) -> io::Result<()> {
    let Stage2MountExpectation {
        filesystems,
        etc,
        runtime_aliases,
        pulse,
        pulse_socket_mode,
    } = expected;
    let application = filesystems.is_some();
    if fs::symlink_metadata(OLD_ROOT).is_ok()
        || (!application && fs::symlink_metadata("/etc").is_ok())
    {
        return Err(io::Error::other(
            "detached host root remains reachable in the fresh root",
        ));
    }
    for (path, expected) in
        grant_scaffold_names(application, runtime_aliases, filesystems.unwrap_or_default())?
    {
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
        if runtime_aliases {
            require_runtime_aliases()?;
        }
        require_etc_plan(&mountinfo, token, etc, identity)?;
        require_names("/run", required_run_names(pulse))?;
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
        require_mode("/home/td/.cache/tmp", 0o700)?;
        require_writable_directory("/home/td/.cache/tmp", token)?;
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
        if pulse {
            require_names("/run/flatpak", &["pulse"])?;
            require_names("/run/flatpak/pulse", &["config", "native", "server"])?;
            require_mode("/run/flatpak", 0o755)?;
            require_mode("/run/flatpak/pulse", 0o755)?;
            require_mode(crate::permissions::APPLICATION_PULSE_CONFIG_PATH, 0o444)?;
            if fs::read_to_string(crate::permissions::APPLICATION_PULSE_CONFIG_PATH)?
                != crate::permissions::APPLICATION_PULSE_CONFIG
            {
                return Err(io::Error::other(
                    "private PulseAudio client configuration changed after mounting",
                ));
            }
            if fs::read_link(crate::permissions::APPLICATION_PULSE_SOCKET_PATH)?
                != Path::new("server/native")
            {
                return Err(io::Error::other(
                    "private PulseAudio endpoint alias changed after mounting",
                ));
            }
            let socket = fs::metadata(crate::permissions::APPLICATION_PULSE_SOCKET_PATH)?;
            if !socket.file_type().is_socket() {
                return Err(io::Error::other(
                    "private PulseAudio endpoint is not a socket",
                ));
            }
            if let Some(mode) = pulse_socket_mode {
                require_mode(crate::permissions::APPLICATION_PULSE_SOCKET_PATH, mode)?;
            }
            require_mount(
                &mountinfo,
                "/run/flatpak/pulse/server",
                None,
                &["ro", "nosuid", "nodev", "noexec"],
                &["rw"],
            )?;
            require_mount(
                &mountinfo,
                "/run/flatpak",
                None,
                &["ro", "nosuid", "nodev", "noexec"],
                &["rw"],
            )?;
            require_read_only_mount("/run/flatpak", token)?;
            require_read_only_mount("/run/flatpak/pulse", token)?;
        }
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

fn require_runtime_aliases() -> io::Result<()> {
    for (name, expected_target) in RUNTIME_ALIASES {
        let alias = PathBuf::from("/").join(name);
        let alias_error = |action: &str, error: io::Error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{action} runtime alias {} to {expected_target}: {error}",
                    alias.display()
                ),
            )
        };
        let metadata = fs::symlink_metadata(&alias)
            .map_err(|error| alias_error("inspect", error))?;
        if !metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "runtime alias {} is not a symbolic link to {expected_target}",
                alias.display()
            )));
        }
        let actual_target = fs::read_link(&alias)
            .map_err(|error| alias_error("read", error))?;
        if actual_target.as_os_str() != OsStr::new(expected_target) {
            return Err(io::Error::other(format!(
                "runtime alias {} points to {}, expected {expected_target}",
                alias.display(),
                actual_target.display()
            )));
        }
        let target_metadata = fs::metadata(expected_target)
            .map_err(|error| alias_error("inspect target for", error))?;
        if !target_metadata.file_type().is_dir() {
            return Err(io::Error::other(format!(
                "runtime alias {} target {expected_target} is not a directory",
                alias.display()
            )));
        }
    }
    Ok(())
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

/// One live stage-1/stage-2 pair and the three handles stage 1 must keep to
/// hold it that way.
struct ProbeInstance {
    child: Child,
    /// The proof pipe's write end (§C). Stage 2's liveness watcher reads the
    /// other end, so CLOSING this is how stage 1 says "I am gone" — and §H
    /// item 12 is the proof that stage 1 dying says it too. A role that
    /// drops this has ended its own instance.
    proof_writer: io::PipeWriter,
    stage2_output: io::PipeReader,
}

/// The namespace transition `--probe-transition` and §H item 12 share.
///
/// One function rather than two because item 12's whole claim is about what
/// stage 1's death does to a REAL transition: a kill-reaps role that built
/// its instance a little differently would be proving the property about
/// something the production path does not have.
///
/// Returns with the token already through the pipe, so stage 2 is released
/// and running. The caller decides what to do with the write end, which is
/// the only thing the two roles disagree about.
fn start_probe_instance(stage2_action: &str) -> io::Result<ProbeInstance> {
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
    NamespaceSnapshot::read()?.require_application_change(&before, true)?;
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
        .arg(stage2_action)
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
    Ok(ProbeInstance {
        child,
        proof_writer,
        stage2_output,
    })
}

pub fn probe_transition() -> io::Result<()> {
    let ProbeInstance {
        mut child,
        proof_writer,
        stage2_output,
    } = start_probe_instance(STAGE2_PROBE_ARG)?;
    // This action's stage 2 runs to completion and is read to EOF, so the
    // proof pipe has done its whole job once the token is through. The
    // kill-reaps role is the one for which holding it open is the point.
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

/// What the §H item 12 driver saw, so its diagnostic can name it.
struct KillReapsObservation {
    stage1: u32,
    stage2: u32,
    descendant: u32,
    descendant_namespace_pid: u32,
    waited: Duration,
}

/// §H item 12: `kill -KILL` of stage 1 reaps the whole instance.
///
/// The driver spawns the stage-1 role, learns stage 2's host pid from it,
/// resolves the descendant's host pid itself by walking its own `/proc`,
/// SIGKILLs stage 1, and then waits for BOTH to stop executing. The only
/// signal it sends goes to stage 1, so what happens to the other two is a
/// consequence of the teardown rather than of anything the driver did.
///
/// Note the verb, because an earlier version of this paragraph got it
/// wrong twice. The claim is NOT that the driver could not signal them:
/// a process in an ancestor pid namespace with a matching uid can signal
/// by host pid, and the driver holds both pids precisely so it can watch
/// them. The claim is that this code does not.
///
/// The descendant is what makes it "the whole instance" rather than
/// "stage 2": the driver never spawned it, stage 2 did, inside the
/// namespace.
///
/// What this does NOT prove is §C's other sentence — that the independent
/// cgroup watcher drains the leaf. This instance has no cgroup: the only
/// shipped application is Firefox, whose leaf `--probe-resource-caps`
/// already reads live, and creating a second `firefox-` leaf beside it would
/// break the one-active-instance rule that probe depends on.
///
/// That half is UNPROVEN, not covered elsewhere, and an earlier version of
/// this comment said otherwise. `remove_abandoned` has source-text pins on
/// its ordering rather than a test that drains a populated leaf, and
/// `--probe-resource-caps` reads a leaf while it is still populated, which
/// is the opposite end of the lifecycle. Proving it needs a jailed instance
/// with a cgroup that this probe is allowed to kill, which is what the
/// single-application rule currently denies.
pub fn probe_kill_reaps() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the kill-reaps probe requires the nonzero application identity",
        ));
    }
    start_probe_watchdog("the kill-reaps driver")?;
    let executable = std::env::current_exe()?;
    let mut stage1 = Command::new(executable)
        .arg(KILL_REAPS_STAGE1_ARG)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn the kill-reaps stage-1 role: {e}")))?;
    // Taken before anything can fail, so the cleanup below covers every
    // path out of this function. As a `?` after the spawn this was the one
    // early return that left a held stage 1 running for its whole ceiling.
    let Some(mut reports) = stage1.stdout.take() else {
        let _ = stage1.kill();
        let _ = stage1.wait();
        return Err(io::Error::other(
            "kill-reaps stage-1 stdout pipe was not created",
        ));
    };
    let observed = observe_kill_reaps(&mut stage1, &mut reports);
    // For the paths that failed BEFORE the kill: the stage-1 role holds its
    // instance open on purpose, so an early return that left it running
    // would leak a held jail into the rest of the boot. `Child::kill` after
    // a successful observation is a no-op that reports an error, which is
    // why this discards the result rather than reading it.
    let _ = stage1.kill();
    let _ = stage1.wait();
    let observed = observed?;
    writeln!(
        io::stdout(),
        "TD-JAIL-KILL-REAPS stage-1={} stage-2={} descendant={} \
         descendant-namespace-pid={} waited-ms={}",
        observed.stage1,
        observed.stage2,
        observed.descendant,
        observed.descendant_namespace_pid,
        observed.waited.as_millis()
    )?;
    writeln!(io::stdout(), "{KILL_REAPS_MARKER}")
}

/// Takes the report stream separately from the process rather than reading
/// `stage1.stdout` itself, so a caller can drive it over a helper reporting
/// on a different descriptor.
///
/// No host test does: one was written and could not survive libtest, which
/// runs tests off the main thread while `CLONE_NEWUSER` demands a
/// single-threaded caller. So this function's own composition -- that it
/// calls the shape check and the signal check at all -- is pinned in the
/// td-jail recipe's source assertions rather than by a test, the same way
/// the crate pins its other orderings the compiler cannot express.
fn observe_kill_reaps(
    stage1: &mut Child,
    reports: &mut impl Read,
) -> io::Result<KillReapsObservation> {
    let report = read_report_line(reports, "the kill-reaps stage-1 role")?;
    let (stage2, namespace_pid) = parse_kill_reaps_report(&report)?;
    // Resolved HERE, not in stage 1. The driver never unshares, so this is
    // the last process in the chain that still sees the host's `/proc`.
    let descendant = find_jailed_descendant(stage2, namespace_pid)?;
    // Everything up to the kill runs first, so a failure here reports what
    // the probe could not establish rather than blaming the teardown for it.
    let before_stage2 = require_live("stage 2", stage2)?;
    let before_descendant = require_live("the jailed descendant", descendant)?;
    require_instance_shape(stage2, descendant, before_stage2, before_descendant)?;

    // Read both witnesses again, as late as possible. An instance that
    // ended ITSELF between the reads above and the kill below would leave
    // exactly the evidence a successful teardown leaves, and the probe
    // would credit the kill for it. This does not close that window --
    // nothing available here can, since the two events are only ordered in
    // time and not causally observable -- but it shrinks it to the gap
    // between two adjacent statements, from a gap that had the whole shape
    // check in it.
    let confirm_stage2 = require_live("stage 2", stage2)?;
    let confirm_descendant = require_live("the jailed descendant", descendant)?;
    require_instance_shape(stage2, descendant, confirm_stage2, confirm_descendant)?;
    if confirm_stage2.starttime != before_stage2.starttime
        || confirm_descendant.starttime != before_descendant.starttime
    {
        return Err(io::Error::other(
            "the instance's pids were reused between the two readings, so the processes about \
             to be measured are not the ones that were checked",
        ));
    }

    let stage1_pid = stage1.id();
    // The one kill, and the reason this probe spawns stage 1 rather than
    // finding one: `Child::kill` is safe std and sends exactly SIGKILL.
    stage1.kill()?;
    require_killed_by_signal(stage1.wait()?)?;

    let started = Instant::now();
    let deadline = started + KILL_REAPS_TIMEOUT;
    loop {
        let stage2_gone = is_gone(stage2, before_stage2.starttime)?;
        let descendant_gone = is_gone(descendant, before_descendant.starttime)?;
        if stage2_gone && descendant_gone {
            return Ok(KillReapsObservation {
                stage1: stage1_pid,
                stage2,
                descendant,
                descendant_namespace_pid: before_descendant.namespace_pid,
                waited: started.elapsed(),
            });
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "SIGKILL of stage 1 ({stage1_pid}) did not reap the whole instance within \
                 {KILL_REAPS_TIMEOUT:?}: stage 2 ({stage2}) is {}, its jailed descendant \
                 ({descendant}) is {}",
                if stage2_gone { "gone" } else { "still live" },
                if descendant_gone { "gone" } else { "still live" },
            )));
        }
        std::thread::sleep(KILL_REAPS_POLL);
    }
}

/// What the two readings must say before killing stage 1 means anything.
///
/// Separate from the driver because a namespace the host cannot create is
/// exactly the thing these branches are about: `--probe-transition` cannot
/// run outside the target either, so without this every one of these
/// rejections would first be exercised by a QEMU boot, and would show up
/// there as a boot failure rather than as a named wrong assumption.
fn require_instance_shape(
    stage2: u32,
    descendant: u32,
    stage2_process: cgroup::HostProcess,
    descendant_process: cgroup::HostProcess,
) -> io::Result<()> {
    // Alive, not merely present. A process that has already exited still
    // has a `/proc` entry until someone waits for it, and watching such a
    // one "go" proves nothing about the kill -- it was gone before it.
    for (role, pid, process) in [
        ("stage 2", stage2, stage2_process),
        ("the jailed descendant", descendant, descendant_process),
    ] {
        if process.state == 'Z' {
            return Err(io::Error::other(format!(
                "{role} ({pid}) had already exited before the kill, so its disappearance \
                 would not be evidence about one"
            )));
        }
    }
    if descendant_process.parent != stage2 {
        return Err(io::Error::other(format!(
            "the reported descendant {descendant} is a child of {}, not of stage 2 ({stage2})",
            descendant_process.parent
        )));
    }
    if stage2_process.namespace_pid != 1 {
        return Err(io::Error::other(format!(
            "the reported stage 2 ({stage2}) answers to pid {} inside its own namespace, not 1",
            stage2_process.namespace_pid
        )));
    }
    // A descendant whose innermost pid is its host pid is in no namespace of
    // its own, so its death would prove nothing about namespace teardown —
    // and this is the check that fails if the whole instance was built
    // outside a pid namespace by mistake.
    if descendant_process.namespace_pid == descendant || descendant_process.namespace_pid <= 1 {
        return Err(io::Error::other(format!(
            "the reported descendant {descendant} answers to pid {} inside its own namespace, \
             so it is not inside the jail's",
            descendant_process.namespace_pid
        )));
    }
    Ok(())
}

/// Stage 1 must have died BY the probe's signal, not on its own.
///
/// A stage 1 that was already leaving proves nothing about killing one, and
/// without this the probe would count it: the pids vanish either way, so
/// every check after it would pass on evidence about the wrong event.
///
/// Its own function because reaching this line for real needs an instance
/// the host cannot build, while an `ExitStatus` from a killed child is
/// something the host can produce exactly.
fn require_killed_by_signal(status: ExitStatus) -> io::Result<()> {
    if status.signal() == Some(sys::SIGKILL) {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "the kill-reaps stage-1 role ended as {status}, not by SIGKILL — it exited on its \
         own, so nothing measured after it would be about what killing it does"
    )))
}

/// Whether the process this pid named at `starttime` is gone.
///
/// Three ways, and the probe needs all three because only the first is the
/// obvious one:
///
/// - its `/proc` entry is gone;
/// - the pid came back with a different start time, so it is a DIFFERENT
///   process and the one being measured is gone. Without this the probe
///   would wait out its deadline on pid reuse in a busy boot and report a
///   teardown that never failed;
/// - it is a ZOMBIE. Stage 2 is orphaned the moment stage 1 dies, so
///   whether it lingers in `Z` depends on how quickly whichever process
///   adopts it gets round to waiting — which is not a property of the
///   teardown under test. A zombie has exited and holds nothing, so
///   counting it as live would fail this probe for someone else's
///   scheduling. `killing_the_waiting_parent_kills_the_detached_application_session`
///   reads the same field for the same reason.
fn is_gone(pid: u32, starttime: u64) -> io::Result<bool> {
    Ok(cgroup::host_process(pid)?
        .is_none_or(|process| process.starttime != starttime || process.state == 'Z'))
}

fn require_live(role: &str, pid: u32) -> io::Result<cgroup::HostProcess> {
    if pid <= 1 {
        return Err(io::Error::other(format!(
            "the kill-reaps stage-1 role reported {pid} for {role}"
        )));
    }
    cgroup::host_process(pid)?
        .ok_or_else(|| io::Error::other(format!("{role} ({pid}) was already gone before the kill")))
}

/// Parse stage 1's report: stage 2's HOST pid, then the descendant's
/// NAMESPACE pid.
///
/// The two are not interchangeable and the second is not a host pid at all.
/// Stage 1 cannot supply a host pid for the descendant -- see the comment in
/// `run_kill_reaps_stage_1` -- so the driver resolves that itself, using this
/// namespace pid as the cross-check.
fn parse_kill_reaps_report(report: &str) -> io::Result<(u32, u32)> {
    let mut fields = report.split(' ');
    let stage2 = parse_reported_pid(fields.next(), "stage 2")?;
    let namespace_pid = parse_reported_pid(fields.next(), "the jailed descendant's namespace")?;
    if fields.next().is_some() {
        return Err(io::Error::other(format!(
            "the kill-reaps stage-1 role reported more than two values: {report:?}"
        )));
    }
    // Pid 1 of the jail is stage 2. A descendant reported as pid 1 is stage 2
    // named twice, and the whole point of the descendant is that it is a
    // SECOND process, so this is refused rather than measured.
    if namespace_pid <= 1 {
        return Err(io::Error::other(format!(
            "the kill-reaps stage-1 role reported the descendant as namespace pid \
             {namespace_pid}, which is stage 2 itself rather than a process it spawned: \
             {report:?}"
        )));
    }
    Ok((stage2, namespace_pid))
}

fn parse_reported_pid(field: Option<&str>, role: &str) -> io::Result<u32> {
    let field = field
        .ok_or_else(|| io::Error::other(format!("the kill-reaps report has no pid for {role}")))?;
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::other(format!(
            "the kill-reaps report gave {role} an invalid pid: {field:?}"
        )));
    }
    field
        .parse::<u32>()
        .map_err(|error| io::Error::other(format!("the kill-reaps {role} pid is invalid: {error}")))
}

/// Read one newline-terminated report from a writer that stays alive after it.
///
/// A byte at a time, because both usual shapes are wrong here: `read_to_end`
/// waits for an EOF that arrives only when the process being measured dies,
/// and a buffered fill blocks for bytes that are not coming.
fn read_report_line(reader: &mut impl Read, role: &str) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::other(format!(
                    "{role} closed its report without a line terminator"
                )));
            }
            Ok(_) if byte[0] == b'\n' => {
                return String::from_utf8(line).map_err(|error| {
                    io::Error::other(format!("{role} report is not UTF-8: {error}"))
                });
            }
            Ok(_) => {
                if line.len() as u64 >= KILL_REAPS_REPORT_LIMIT {
                    return Err(io::Error::other(format!(
                        "{role} report exceeded {KILL_REAPS_REPORT_LIMIT} bytes"
                    )));
                }
                line.push(byte[0]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// §H item 12's stage 1: build a real instance, name it to the driver, then
/// hold the proof pipe open and wait to be killed.
///
/// It reports ONE host pid and ONE namespace pid, and the asymmetry is the
/// whole point. Stage 2's host pid is usable because
/// `unshare(CLONE_NEWPID)` does not move the caller: stage 1 stays in the
/// outer namespace, so the pid `Command::spawn` handed it is one the driver
/// can look up in `/proc`. §C says the same thing about the pid stage 1
/// registers with the broker.
///
/// The descendant's is a NAMESPACE pid, because stage 1 has no way to learn
/// its host one. By the time stage 2 reports, stage 2 has pivoted -- in the
/// mount namespace stage 1 created and shares -- and `pivot_root(2)` re-roots
/// every process in it, so stage 1's `/proc` is the jail's. The driver
/// resolves the host pid itself, and this is the cross-check it resolves it
/// against.
pub fn run_kill_reaps_stage_1(expected_parent: u32) -> io::Result<()> {
    // Before anything else, and this is not only tidiness. Stage 1 inherits
    // the driver's stderr, which under the boot leg is the pipe a `$(...)`
    // substitution is reading -- so a stage 1 that outlives the driver
    // holds that substitution open on a probe that has already finished.
    // The driver's watchdog exits without killing anything, which is
    // exactly the path that would do it.
    //
    // It is also the ONLY bound this role has, and it has to be, because
    // stage 1 cannot run a watchdog thread at all. `CLONE_NEWUSER` refuses a
    // multi-threaded caller, so no thread may exist BEFORE the unshare; and
    // after `unshare(CLONE_NEWPID)` the kernel refuses `CLONE_THREAD` while
    // a task's `pid_ns_for_children` differs from its active pid namespace,
    // which for an unshared-but-not-yet-forked task it always does. There is
    // no moment in between. So the driver holds the only deadline and this
    // signal carries it here: when the driver's watchdog exits, stage 1 dies
    // with it, and stage 1 dying is what tears the instance down.
    sys::set_parent_death_signal()?;
    // Armed, then CHECKED, because `PR_SET_PDEATHSIG` is not retroactive: a
    // driver that died between this process's exec and the line above sends
    // nothing, and an orphaned stage 1 holds the boot leg's command
    // substitution open for the rest of its life -- the exact hang the
    // signal was added to prevent. This is the same arm-then-check the
    // application launcher does. Read before the unshare, while `/proc` is
    // still the host's and this pid still means what the driver meant.
    let stat = fs::read_to_string("/proc/self/stat")?;
    let observed = process_containment(&stat)?.parent;
    if observed != expected_parent {
        return Err(io::Error::other(format!(
            "the kill-reaps stage-1 role expected the driver ({expected_parent}) as its \
             parent and read {observed}, so the driver is already gone and nothing is \
             waiting for the instance this would build"
        )));
    }
    let ProbeInstance {
        child,
        proof_writer,
        mut stage2_output,
    } = start_probe_instance(STAGE2_KILL_HOLD_ARG)?;
    let stage2 = child.id();
    let hold = read_report_line(&mut stage2_output, "the held stage 2")?;
    let namespace_pid = parse_stage2_hold(&hold)?;
    // Report, do not resolve. Stage 1 shares the mount namespace that stage
    // 2 pivots in -- stage 2 unshares nothing -- and `pivot_root(2)` re-roots
    // EVERY process in that namespace, stage 1 included. So by the time this
    // hold line arrives, stage 1's `/proc` is no longer the host's: it is the
    // jail's procfs, bound to the new pid namespace, holding stage 2 as pid 1
    // and the descendant and nothing else.
    //
    // An earlier version walked `/proc` HERE for a host pid. A host run of it
    // read exactly those two namespace entries, found nothing whose parent
    // was stage 2's host pid, and reported that stage 2 had never spawned a
    // descendant. The driver never unshares and still holds the host view, so
    // that walk belongs there; stage 1 forwards only what it alone knows --
    // stage 2's host pid, and the namespace pid stage 2 named itself.
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{stage2} {namespace_pid}")?;
    stdout.flush()?;
    drop(stdout);
    // Held to the end of the process, both on purpose: `proof_writer` is the
    // pipe whose closing tears the instance down, and `child` is the stage 2
    // this role must not reap. Dying is what releases them, which is the
    // whole experiment.
    let held = (child, proof_writer);
    let outcome = hold_until_reaped("the kill-reaps stage-1 role");
    drop(held);
    outcome
}

fn parse_stage2_hold(line: &str) -> io::Result<u32> {
    let prefix = format!("{STAGE2_HOLD_MARKER} descendant=");
    let namespace_pid = line.strip_prefix(&prefix).ok_or_else(|| {
        io::Error::other(format!("the held stage 2 reported {line:?}, not {prefix:?}"))
    })?;
    if namespace_pid.is_empty() || !namespace_pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::other(format!(
            "the held stage 2 reported an invalid descendant pid: {namespace_pid:?}"
        )));
    }
    namespace_pid.parse::<u32>().map_err(|error| {
        io::Error::other(format!("the held stage 2 descendant pid is invalid: {error}"))
    })
}

/// The host pid of stage 2's one child, cross-checked against the pid stage 2
/// knows it by.
///
/// The `NSpid` check is what makes this an identification rather than a
/// guess: stage 2 reported a number from inside the namespace and this
/// resolves a number outside it, so a walk that landed on the wrong process
/// fails here instead of being killed and counted.
///
/// A `/proc` entry that cannot be read is skipped rather than refused,
/// which is safe for the case this probe actually has: stage 2 spawns
/// exactly one child, so missing it leaves none and the probe fails. It is
/// NOT safe in general -- were stage 2 ever to have two children and one of
/// them unreadable, this would find the other and accept it. The
/// exactly-one requirement below is what would otherwise have caught that,
/// so the two are load-bearing together rather than separately.
fn find_jailed_descendant(stage2: u32, namespace_pid: u32) -> io::Result<u32> {
    let mut children = Vec::new();
    let mut seen = 0usize;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        seen = seen.saturating_add(1);
        if seen > MAX_PROC_SCAN {
            return Err(io::Error::other(format!(
                "/proc holds more than {MAX_PROC_SCAN} entries"
            )));
        }
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(Some(process)) = cgroup::host_process(pid) else {
            continue;
        };
        if process.parent == stage2 {
            children.push((pid, process.namespace_pid));
        }
    }
    let (pid, found_namespace_pid) = match children.as_slice() {
        [] => {
            return Err(io::Error::other(format!(
                "stage 2 ({stage2}) has no child in the host /proc: either it never spawned \
                 the descendant this probe measures, or the walk could not read the entry \
                 that is it"
            )));
        }
        [only] => *only,
        many => {
            return Err(io::Error::other(format!(
                "stage 2 ({stage2}) has {} children in the host /proc, so the descendant is \
                 ambiguous; this probe's stage 2 spawns exactly one",
                many.len()
            )));
        }
    };
    if found_namespace_pid != namespace_pid {
        return Err(io::Error::other(format!(
            "stage 2's child {pid} answers to pid {found_namespace_pid} inside the namespace, \
             but stage 2 reported spawning {namespace_pid}"
        )));
    }
    Ok(pid)
}

/// Stage 2's §H item 12 role: become the instance a `kill -KILL` of stage 1
/// must reap, then wait to be reaped.
///
/// It starts the SAME liveness watcher `Launch` does rather than a
/// probe-shaped imitation, because that watcher is the mechanism under test:
/// stage 1's death closes fd 0, the watcher's read returns, and PID 1 exits,
/// which is what makes the kernel tear the namespace down. `PR_SET_PDEATHSIG`
/// was armed at the top of `run_stage2` and races it to the same end; the
/// driver observes the OUTCOME, so which one wins does not matter and is not
/// claimed either way.
fn run_stage2_kill_hold() -> io::Result<()> {
    start_stage1_liveness_watcher()?;
    let child = Command::new(REAPER_PROBE_PATH)
        .arg(KILL_HOLD_CHILD_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn the kill-hold descendant: {e}")))?;
    let descendant = child.id();
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{STAGE2_HOLD_MARKER} descendant={descendant}")?;
    stdout.flush()?;
    drop(stdout);
    // Held, never waited for: PID 1 reaping it would remove the very process
    // whose disappearance is the evidence.
    let outcome = hold_until_reaped("the held stage 2");
    drop(child);
    outcome
}

pub fn run_kill_hold_child() -> io::Result<()> {
    hold_until_reaped("the kill-hold descendant")
}

/// Bound a role that is about to block on a read nothing may ever answer.
///
/// Exits the process rather than unwinding: the point is that the caller is
/// stuck inside a blocking read and cannot be returned to. A non-zero exit
/// is what the boot leg reads as the probe having failed, which is what it
/// is -- and the diagnostic says which role gave up, because "the probe
/// timed out" without that is three different faults spelled the same way.
///
/// Only the driver may arm this, and that is a kernel constraint rather than
/// a choice. `CLONE_NEWUSER` refuses a multi-threaded caller, so a role that
/// unshares may hold no thread beforehand; and after `unshare(CLONE_NEWPID)`
/// the kernel refuses `CLONE_THREAD` outright while `pid_ns_for_children`
/// differs from the active pid namespace, which is exactly the state an
/// unshared-but-not-yet-forked task is in. An earlier version of this
/// function was armed by stage 1 "after the unshare"; a host run of that
/// role reds `clone(CLONE_THREAD...) = EINVAL`, surfacing through musl as
/// EAGAIN, on every attempt. Stage 1 is bounded by this deadline reaching it
/// through `PR_SET_PDEATHSIG` instead of by a watchdog of its own.
fn start_probe_watchdog(role: &'static str) -> io::Result<()> {
    let watchdog = std::thread::Builder::new()
        .name("td-jail-kill-reaps-watchdog".to_string())
        .spawn(move || {
            std::thread::sleep(KILL_REAPS_CEILING);
            let _ = writeln!(
                io::stderr(),
                "{role} gave up after {KILL_REAPS_CEILING:?} without reaching a verdict"
            );
            std::process::exit(126);
        })?;
    drop(watchdog);
    Ok(())
}

/// Block until something else ends this process.
///
/// The ceiling is not a timeout on anything: the driver's own deadline is far
/// shorter, so no passing run reaches it. It bounds a role that has REPORTED
/// and is now waiting to be torn down -- and only that. A role that hangs
/// before reporting never arrives here, which is what the DRIVER's
/// `start_probe_watchdog` is for -- reaching stage 2 through stage 1's
/// `PR_SET_PDEATHSIG` and the proof pipe, since neither stage may hold a
/// watchdog thread of its own. An earlier version of this comment credited
/// this ceiling with covering both, and was wrong about the half that
/// matters more: a hang before the report is the one that blocks the boot
/// leg's command substitution.
fn hold_until_reaped(role: &str) -> io::Result<()> {
    let deadline = Instant::now() + KILL_HOLD_CEILING;
    while Instant::now() < deadline {
        std::thread::sleep(KILL_HOLD_POLL);
    }
    Err(io::Error::other(format!(
        "{role} was still alive after the {KILL_HOLD_CEILING:?} kill-reaps hold ceiling"
    )))
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

pub fn probe_process_token(application: &str, token: &str) -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail requires the nonzero application identity",
        ));
    }
    let limits = authority::resolve_resource_limits(application)?;
    let diagnostic =
        cgroup::probe_process_token(application, token, limits, identity.uid, identity.gid)?;
    writeln!(io::stdout(), "{diagnostic}")
}

pub fn probe_firefox_support() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Firefox support probe requires the nonzero application identity",
        ));
    }
    let report = firefox::probe_support()?;
    let limits = authority::resolve_resource_limits("firefox")?;
    let sandboxes = cgroup::process_sandboxes(
        "firefox",
        &firefox::namespace_pids(&report),
        limits,
        identity.uid,
        identity.gid,
    )?;
    let diagnostic = firefox::validate_and_render(&report, &sandboxes)?;
    writeln!(io::stdout(), "{diagnostic}")
}

pub fn probe_firefox_network() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Firefox network probe requires the nonzero application identity",
        ));
    }
    writeln!(io::stdout(), "{}", firefox::probe_network()?)
}

pub fn probe_firefox_soak() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Firefox soak probe requires the nonzero application identity",
        ));
    }
    writeln!(io::stdout(), "{}", firefox::probe_soak()?)
}

pub fn probe_firefox_input(stage: firefox::InputStage) -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Firefox input probe requires the nonzero application identity",
        ));
    }
    let mut stdout = io::stdout().lock();
    let marker = firefox::probe_input(stage, &mut stdout)?;
    writeln!(stdout, "{marker}")
}

pub fn probe_firefox_download() -> io::Result<()> {
    let identity = current_identity()?;
    if identity.uid == 0 || identity.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the Firefox download probe requires the nonzero application identity",
        ));
    }
    writeln!(io::stdout(), "{}", firefox::probe_download()?)
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
    let host_mode = matches!(
        &action,
        Stage2Action::Launch(launch) if launch.cgroup_membership == NO_CGROUP_MEMBERSHIP
    );
    let (
        filesystems,
        etc,
        runtime_aliases,
        pulse,
    ) = match &action {
        Stage2Action::Probe | Stage2Action::KillHold => (
            None,
            EtcBinding {
                resolv_conf: false,
                timezone: None,
                firefox_autotest_policy: false,
                machine_id: None,
            },
            false,
            false,
        ),
        Stage2Action::Launch(launch) => (
            Some(launch.filesystems.as_slice()),
            EtcBinding {
                resolv_conf: launch.resolv_conf,
                timezone: launch.timezone.as_deref(),
                firefox_autotest_policy: launch.firefox_autotest_policy,
                machine_id: Some(launch.machine_id.as_str()),
            },
            launch.runtime_aliases,
            // Stage 1 already authenticated the permission-to-environment
            // derivation. This bit checks only that the corresponding private
            // mount survived the stage-2 transition.
            launch
                .environment
                .iter()
                .any(|(key, _)| key == "PULSE_SERVER"),
        ),
    };
    require_mount_plan(
        Stage2MountExpectation {
            filesystems,
            etc,
            runtime_aliases,
            pulse,
            pulse_socket_mode: pulse_socket_mode(pulse, host_mode),
        },
        &mount_probe_token,
        identity,
    )?;
    clear_and_require_empty_capabilities()?;
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
        Stage2Action::KillHold => run_stage2_kill_hold(),
        Stage2Action::Launch(launch) => {
            // Every field named, none elided. The mount-plan facts are
            // already spent above, but a `..` here would let the NEXT field
            // be added and silently never reach this frame — which is the
            // mistake this very commit had to be threaded through twice to
            // avoid making.
            let Stage2Launch {
                entry,
                resolv_conf: _,
                machine_id: _,
                timezone: _,
                firefox_autotest_policy: _,
                runtime_aliases: _,
                environment,
                filesystems: _,
                resources,
                cgroup_membership,
                arguments,
            } = *launch;
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
    if let Some(socket) = &application.pulse_socket {
        let what = if application.host_mode {
            "host PulseAudio authority"
        } else {
            "td-audio authority"
        };
        crate::bus::require_accepting_endpoint(socket, PULSE_CONNECT_TIMEOUT, what)?;
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
    // the honest statement that this instance activates none.
    //
    // The names it may OWN are not empty and are not this crate's to choose:
    // they are the permission file's `own` entries, which reach the broker
    // here because phase one is the only place a jail speaks to it about the
    // instance rather than on the instance's behalf.
    let registration = crate::bus::register(
        &application.bus_socket,
        outside_identity.uid,
        crate::bus::Registration {
            instance: &instance,
            app_id: &application.name,
            services: &[],
            owned: &application.owned_names,
        },
    )
    .map_err(|e| io::Error::other(format!("register jail instance {instance:?}: {e}")))?;

    if application.host_mode {
        writeln!(io::stderr(), "{HOST_DEGRADATION_CGROUP}")?;
        writeln!(io::stderr(), "{HOST_DEGRADATION_WAYLAND}")?;
    }

    let launch_result = (|| -> io::Result<()> {
        sys::unshare_namespaces(application.isolate_network).map_err(|error| {
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
        NamespaceSnapshot::read()?
            .require_application_change(&before, application.isolate_network)?;
        if application.isolate_network {
            sys::bring_up_loopback()
                .map_err(|e| io::Error::other(format!("bring up isolated loopback: {e}")))?;
        }
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
            Stage2MountBinding {
                filesystems: &application.filesystems,
                resolv_conf: application.resolv_conf.is_some(),
                machine_id: &application.state.machine_id,
                timezone: application
                    .timezone
                    .as_ref()
                    .map(|zone| zone.name.as_str()),
                firefox_autotest_policy: application
                    .firefox_autotest_policy
                    .is_some(),
                loader_library_path: application.loader_library_path.as_deref(),
            },
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

    fn namespace_snapshot(
        user: &str,
        mount: &str,
        uts: &str,
        network: &str,
    ) -> NamespaceSnapshot {
        NamespaceSnapshot {
            user: PathBuf::from(user),
            mount: PathBuf::from(mount),
            uts: PathBuf::from(uts),
            network: PathBuf::from(network),
            pid: PathBuf::from("pid:[1]"),
        }
    }

    #[test]
    fn application_namespace_readback_distinguishes_shared_and_isolated_networks() {
        let before = namespace_snapshot("user:[1]", "mnt:[1]", "uts:[1]", "net:[1]");
        let isolated = namespace_snapshot("user:[2]", "mnt:[2]", "uts:[2]", "net:[2]");
        let shared = namespace_snapshot("user:[2]", "mnt:[2]", "uts:[2]", "net:[1]");

        isolated.require_application_change(&before, true).unwrap();
        shared.require_application_change(&before, false).unwrap();
        assert!(shared.require_application_change(&before, true).is_err());
        assert!(isolated
            .require_application_change(&before, false)
            .is_err());

        for unchanged in [
            namespace_snapshot("user:[1]", "mnt:[2]", "uts:[2]", "net:[2]"),
            namespace_snapshot("user:[2]", "mnt:[1]", "uts:[2]", "net:[2]"),
            namespace_snapshot("user:[2]", "mnt:[2]", "uts:[1]", "net:[2]"),
        ] {
            assert!(unchanged
                .require_application_change(&before, true)
                .is_err());
        }
    }

    #[test]
    fn pulse_mount_plan_is_absent_or_uses_the_compiled_private_paths() {
        assert_eq!(required_run_names(false), ["user"]);
        assert_eq!(required_run_names(true), ["flatpak", "user"]);
        assert_eq!(pulse_mount_plan(None), None);
        let source = Path::new("/run/td-audio");
        assert_eq!(
            pulse_mount_plan(Some(source)),
            Some(PulseMountPlan {
                source,
                runtime_target: format!("{NEW_ROOT}/run/flatpak/pulse/server"),
                config_target: format!(
                    "{NEW_ROOT}{}",
                    crate::permissions::APPLICATION_PULSE_CONFIG_PATH
                ),
            })
        );
        assert_eq!(
            crate::permissions::APPLICATION_PULSE_CONFIG,
            "autospawn = no\nenable-shm = no\n"
        );
        assert_eq!(pulse_socket_mode(false, false), None);
        assert_eq!(pulse_socket_mode(true, true), None);
        assert_eq!(
            pulse_socket_mode(true, false),
            Some(crate::permissions::TD_AUDIO_SOCKET_MODE)
        );
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
        let machine_id = "0123456789abcdef0123456789abcdef\n";
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
                STAGE2_RESOLV_CONF_ARG,
                "present",
                STAGE2_MACHINE_ID_ARG,
                machine_id,
                STAGE2_TIMEZONE_ARG,
                "Europe/Berlin",
                STAGE2_FIREFOX_AUTOTEST_POLICY_ARG,
                "present",
                STAGE2_LOADER_LIBRARY_PATH_ARG,
                "absent",
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
                "50000",
                "100000",
                "/td-user-1000/app-0123456789abcdef",
                STAGE2_ARGUMENTS_ARG,
                "--flag",
            ]))
            .unwrap(),
            Mode::Stage2 {
                action: Stage2Action::Launch(launch),
                ..
            } if *launch == Stage2Launch {
                entry: "/app/bin/app".into(),
                resolv_conf: true,
                machine_id: machine_id.into(),
                timezone: Some("Europe/Berlin".into()),
                firefox_autotest_policy: true,
                runtime_aliases: false,
                environment: [
                    ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                    ("FLATPAK_ID", "org.td.App"),
                    ("GLIBC_TUNABLES", "glibc.malloc.perturb=1"),
                    ("HOME", "/home/td"),
                    ("WAYLAND_DISPLAY", "wayland-0"),
                    ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ]
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .to_vec(),
                filesystems: vec![Stage2Filesystem {
                    target: PathBuf::from("/home/td/Downloads"),
                    read_only: false,
                    source_kind: FilesystemSourceKind::Directory,
                }],
                resources: ResolvedResourceLimits {
                    memory_high_bytes: 50_331_648,
                    memory_max_bytes: 67_108_864,
                    pids_max: 32,
                    cpu_quota_usec: 50_000,
                    cpu_period_usec: 100_000,
                },
                cgroup_membership: "/td-user-1000/app-0123456789abcdef".into(),
                arguments: vec![OsString::from("--flag")],
            }
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
            STAGE2_RESOLV_CONF_ARG,
            "absent",
            STAGE2_MACHINE_ID_ARG,
            machine_id,
            STAGE2_TIMEZONE_ARG,
            NO_TIMEZONE,
            STAGE2_FIREFOX_AUTOTEST_POLICY_ARG,
            "absent",
            STAGE2_LOADER_LIBRARY_PATH_ARG,
            "absent",
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
            STAGE2_RESOLV_CONF_ARG,
            "absent",
            STAGE2_MACHINE_ID_ARG,
            machine_id,
            STAGE2_TIMEZONE_ARG,
            NO_TIMEZONE,
            STAGE2_FIREFOX_AUTOTEST_POLICY_ARG,
            "absent",
            STAGE2_LOADER_LIBRARY_PATH_ARG,
            "absent",
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
        assert!(parse_mode(args(&[
            STAGE2_ARG,
            &encoded,
            "1000",
            "1000",
            "1000",
            "1000",
            STAGE2_LAUNCH_ARG,
            "/app/bin/app",
            STAGE2_RESOLV_CONF_ARG,
            "conditional",
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
                OsString::from("LD_LIBRARY_PATH"),
                OsString::from("/app/lib:/app/lib/firefox"),
            ),
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
            cpu_quota_usec: 50_000,
            cpu_period_usec: 100_000,
        };
        let membership = "/td-user-1000/app-0123456789abcdef";
        let machine_id = "0123456789abcdef0123456789abcdef\n";
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
            Stage2MountBinding {
                filesystems: &filesystems,
                resolv_conf: false,
                machine_id,
                timezone: Some("Europe/Berlin"),
                firefox_autotest_policy: true,
                loader_library_path: Some("/app/lib:/app/lib/firefox"),
            },
            Stage2ResourceBinding {
                limits: resources,
                membership,
            },
            &arguments,
        );
        // The sentinel is the value EVERY launch emits until a writer for
        // `/etc/timezone` exists, so it needs a POSITIVE assertion: every
        // other `NO_TIMEZONE` in these tests sits in a case that is refused
        // for some other reason, and would stay green if `absent` became a
        // parse error.
        let mut unzoned = emitted.clone();
        let zone_index = unzoned
            .iter()
            .position(|argument| argument == STAGE2_TIMEZONE_ARG)
            .unwrap();
        unzoned[zone_index + 1] = OsString::from(NO_TIMEZONE);
        assert!(matches!(
            parse_mode(unzoned.into_iter()).unwrap(),
            Mode::Stage2 {
                action: Stage2Action::Launch(launch),
                ..
            } if launch.timezone.is_none()
        ));
        // And a zone name that is not one is refused rather than carried.
        let mut misspelled = emitted.clone();
        misspelled[zone_index + 1] = OsString::from("europe/berlin");
        assert!(parse_mode(misspelled.into_iter()).is_err());
        let mut mismatched = emitted.clone();
        let loader_index = mismatched
            .iter()
            .position(|argument| argument == STAGE2_LOADER_LIBRARY_PATH_ARG)
            .unwrap();
        mismatched[loader_index + 1] = OsString::from("absent");
        mismatched.remove(loader_index + 2);
        assert!(parse_mode(mismatched.into_iter()).is_err());
        assert_eq!(
            parse_mode(emitted.into_iter()).unwrap(),
            Mode::Stage2 {
                token,
                identity,
                outside_identity,
                action: Stage2Action::Launch(Box::new(Stage2Launch {
                    entry: "/app/bin/app".into(),
                    resolv_conf: false,
                    machine_id: machine_id.into(),
                    timezone: Some("Europe/Berlin".into()),
                    firefox_autotest_policy: true,
                    runtime_aliases: true,
                    environment,
                    filesystems: vec![Stage2Filesystem {
                        target: PathBuf::from("/home/td/Downloads"),
                        read_only: false,
                        source_kind: FilesystemSourceKind::Directory,
                    }],
                    resources,
                    cgroup_membership: membership.into(),
                    arguments,
                })),
            }
        );
    }

    /// Each §H item 12 role takes exactly the argv it needs and no more,
    /// and every sibling probe arg in this parser has a test saying so.
    /// These had none: a reviewer deleted the extra-argument guard from the
    /// driver's arm and the whole suite stayed green.
    ///
    /// It matters most for the two INTERNAL roles. They are argv the
    /// launcher could be handed, and a role that silently ignored trailing
    /// arguments would accept an invocation nothing in this crate produces.
    #[test]
    fn the_argument_free_kill_reaps_roles_take_no_arguments() {
        for (arg, mode) in [
            (KILL_REAPS_PROBE_ARG, Mode::KillReapsProbe),
            (KILL_HOLD_CHILD_ARG, Mode::KillHoldChild),
        ] {
            assert_eq!(parse_mode(args(&[arg])).unwrap(), mode);
            assert!(
                parse_mode(args(&[arg, "extra"])).is_err(),
                "{arg} must refuse a trailing argument"
            );
        }
    }

    /// Stage 1 takes the driver's pid, and takes it as a POSITIVE pid.
    ///
    /// It is not decoration: stage 1 reads its parent back after arming
    /// `PR_SET_PDEATHSIG` and refuses to build an instance if the driver has
    /// already gone, because that signal is not retroactive. A role that
    /// accepted a missing or junk pid would skip that check.
    #[test]
    fn the_kill_reaps_stage_1_role_takes_the_drivers_pid() {
        assert_eq!(
            parse_mode(args(&[KILL_REAPS_STAGE1_ARG, "4242"])).unwrap(),
            Mode::KillReapsStage1 {
                expected_parent: 4242
            }
        );
        for refused in [
            vec![KILL_REAPS_STAGE1_ARG],
            vec![KILL_REAPS_STAGE1_ARG, "0"],
            vec![KILL_REAPS_STAGE1_ARG, "-1"],
            vec![KILL_REAPS_STAGE1_ARG, "x"],
            vec![KILL_REAPS_STAGE1_ARG, ""],
            vec![KILL_REAPS_STAGE1_ARG, "4242", "extra"],
        ] {
            assert!(
                parse_mode(args(&refused)).is_err(),
                "{refused:?} must not parse as the kill-reaps stage-1 role"
            );
        }
    }

    #[test]
    fn the_kill_hold_stage_2_action_takes_no_arguments() {
        let token = encode_token(&[7_u8; TOKEN_LEN]);
        let base = [STAGE2_ARG, token.as_str(), "1000", "1000", "1000", "1000"];
        let with = |tail: &[&str]| {
            let mut all: Vec<&str> = base.to_vec();
            all.extend_from_slice(tail);
            parse_mode(args(&all))
        };
        assert!(matches!(
            with(&[STAGE2_KILL_HOLD_ARG]).unwrap(),
            Mode::Stage2 {
                action: Stage2Action::KillHold,
                ..
            }
        ));
        assert!(with(&[STAGE2_KILL_HOLD_ARG, "extra"]).is_err());
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

        assert_eq!(
            parse_mode(args(&[PROCESS_TOKEN_PROBE_ARG, "firefox", "-contentproc"])).unwrap(),
            Mode::ProcessTokenProbe {
                application: "firefox".into(),
                token: "-contentproc".into(),
            }
        );
        assert!(parse_mode(args(&[PROCESS_TOKEN_PROBE_ARG, "firefox"])).is_err());
        assert!(parse_mode(args(&[PROCESS_TOKEN_PROBE_ARG, "firefox", "bad token"])).is_err());
        assert!(parse_mode(args(&[
            PROCESS_TOKEN_PROBE_ARG,
            "firefox",
            "-contentproc",
            "extra"
        ]))
        .is_err());

        assert_eq!(
            parse_mode(args(&[FIREFOX_SUPPORT_PROBE_ARG])).unwrap(),
            Mode::FirefoxSupportProbe
        );
        assert!(parse_mode(args(&[FIREFOX_SUPPORT_PROBE_ARG, "extra"])).is_err());
        assert_eq!(
            parse_mode(args(&[FIREFOX_NETWORK_PROBE_ARG])).unwrap(),
            Mode::FirefoxNetworkProbe
        );
        assert!(parse_mode(args(&[FIREFOX_NETWORK_PROBE_ARG, "extra"])).is_err());
        assert_eq!(
            parse_mode(args(&[FIREFOX_SOAK_PROBE_ARG])).unwrap(),
            Mode::FirefoxSoakProbe
        );
        assert!(parse_mode(args(&[FIREFOX_SOAK_PROBE_ARG, "extra"])).is_err());
        for (name, stage) in [
            ("arm", firefox::InputStage::Arm),
            ("menu", firefox::InputStage::Menu),
            ("final", firefox::InputStage::Final),
            (
                "clipboard-refocus-arm",
                firefox::InputStage::ClipboardRefocusArm,
            ),
            (
                "clipboard-refocus",
                firefox::InputStage::ClipboardRefocus,
            ),
            ("clipboard", firefox::InputStage::Clipboard),
            ("download", firefox::InputStage::Download),
            ("file-chooser", firefox::InputStage::FileChooser),
            (
                "file-chooser-focus",
                firefox::InputStage::FileChooserFocus,
            ),
            (
                "file-chooser-result",
                firefox::InputStage::FileChooserResult,
            ),
        ] {
            assert_eq!(
                parse_mode(args(&[FIREFOX_INPUT_PROBE_ARG, name])).unwrap(),
                Mode::FirefoxInputProbe { stage }
            );
        }
        assert!(parse_mode(args(&[FIREFOX_INPUT_PROBE_ARG])).is_err());
        assert!(parse_mode(args(&[FIREFOX_INPUT_PROBE_ARG, "wait"])).is_err());
        assert!(parse_mode(args(&[FIREFOX_INPUT_PROBE_ARG, "arm", "extra"])).is_err());
        assert_eq!(
            parse_mode(args(&[FIREFOX_DOWNLOAD_PROBE_ARG])).unwrap(),
            Mode::FirefoxDownloadProbe
        );
        assert!(parse_mode(args(&[FIREFOX_DOWNLOAD_PROBE_ARG, "extra"])).is_err());
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

    /// §H item 12's driver, in the parts a host can run.
    ///
    /// What it cannot run is the instance: stage 1 unshares `CLONE_NEWUSER`,
    /// which the kernel refuses to a multi-threaded caller, and libtest runs
    /// every test off the main thread — the same constraint §C states as the
    /// reason stage 1 unshares in its own `main` before spawning anything.
    /// A helper that unshared here gets `EINVAL` however the harness is
    /// configured. `--probe-transition` has the same target-only shape.
    ///
    /// So the split is deliberate: the target run proves the teardown, and
    /// these prove the reasoning around it — which is where a wrong
    /// assumption would otherwise first appear as a QEMU boot failure with
    /// no name on it.
    #[test]
    fn kill_hold_descendant_helper() {
        if std::env::var_os("TD_JAIL_TEST_KILL_HOLD_DESCENDANT").is_none() {
            return;
        }
        // Bounded rather than parked forever. One caller kills this
        // directly, but the other kills its PARENT, which orphans it — and
        // an orphan that never returns is a process leaked into the rest of
        // the suite. The bound is far longer than either caller needs and
        // far shorter than a gate run.
        std::thread::sleep(Duration::from_secs(30));
    }

    /// Stands in for stage 2: a process whose ONLY child is the descendant.
    ///
    /// The walk has to be asked about a process with a known child set, and
    /// the TEST process is not one — every other test spawning a subprocess
    /// is another child of it. Asking about this helper instead makes the
    /// answer independent of what the rest of the suite is doing, which is
    /// exactly what the first version of this test got wrong: it passed
    /// filtered and failed under the full gate.
    ///
    /// Reports on stderr because the harness owns stdout, and reports the
    /// pid `Command::spawn` handed it, which outside a pid namespace is
    /// also the pid the descendant answers to — so it is what stage 2
    /// would have reported.
    // Never waits for its child, deliberately: this stands in for a stage 2
    // whose descendant must still be there for the walk to find. The child
    // bounds itself, and the caller kills this whole helper.
    #[allow(clippy::zombie_processes)]
    #[test]
    fn kill_hold_stage2_helper() {
        // The value is how many descendants to spawn: one for the ordinary
        // case, more so the ambiguity branch has something to be ambiguous
        // about. It always reports the FIRST, which is the one a correct
        // walk would have to agree on.
        let Some(count) = std::env::var_os("TD_JAIL_TEST_KILL_HOLD_STAGE2") else {
            return;
        };
        let count: usize = count.to_str().unwrap().parse().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut first = None;
        for _ in 0..count {
            let child = Command::new(&executable)
                .args([
                    "--exact",
                    "transition::tests::kill_hold_descendant_helper",
                    "--nocapture",
                ])
                .env("TD_JAIL_TEST_KILL_HOLD_DESCENDANT", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            first.get_or_insert(child.id());
        }
        writeln!(
            io::stderr(),
            "{STAGE2_HOLD_MARKER} descendant={}",
            first.unwrap()
        )
        .unwrap();
        io::stderr().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn the_jailed_descendant_is_identified_by_parent_and_namespace_pid() {
        let executable = std::env::current_exe().unwrap();
        let mut stage2 = Command::new(executable)
            .args([
                "--exact",
                "transition::tests::kill_hold_stage2_helper",
                "--nocapture",
            ])
            .env("TD_JAIL_TEST_KILL_HOLD_STAGE2", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stage2_pid = stage2.id();
        let mut reports = stage2.stderr.take().unwrap();
        let hold = read_report_line(&mut reports, "the stage-2 stand-in").unwrap();
        let descendant_pid = parse_stage2_hold(&hold).unwrap();

        let found = find_jailed_descendant(stage2_pid, descendant_pid);
        // The cross-check is the point: a walk that found the right parent
        // but the wrong child must fail rather than name it.
        let mismatch = find_jailed_descendant(stage2_pid, descendant_pid.wrapping_add(1));
        // A stage 2 with no child at all is a failure, never a pass.
        let childless = find_jailed_descendant(descendant_pid, 1);

        // The helper holds a child of its own, so killing it first would
        // leave that child parented to the test harness. Take the whole
        // pair down before asserting, so a failing assertion cannot leak a
        // parked process into the rest of the suite.
        stage2.kill().unwrap();
        stage2.wait().unwrap();

        assert_eq!(found.unwrap(), descendant_pid);
        let mismatch = mismatch.unwrap_err();
        assert!(
            mismatch.to_string().contains("but stage 2"),
            "a namespace-pid mismatch must be reported as one: {mismatch}"
        );
        let childless = childless.unwrap_err();
        assert!(
            childless.to_string().contains("has no child"),
            "a childless stage 2 must be reported as one: {childless}"
        );
    }

    #[test]
    fn a_stage_2_with_more_than_one_child_is_ambiguous_rather_than_guessed_at() {
        let executable = std::env::current_exe().unwrap();
        let mut stage2 = Command::new(executable)
            .args([
                "--exact",
                "transition::tests::kill_hold_stage2_helper",
                "--nocapture",
            ])
            // Two, so the walk finds a child the report does not name. It
            // must say so rather than take the one that matches: this
            // probe's stage 2 spawns exactly one, so a second means the
            // instance is not the shape the proof assumes.
            .env("TD_JAIL_TEST_KILL_HOLD_STAGE2", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stage2_pid = stage2.id();
        let mut reports = stage2.stderr.take().unwrap();
        let hold = read_report_line(&mut reports, "the stage-2 stand-in").unwrap();
        let descendant_pid = parse_stage2_hold(&hold).unwrap();
        let ambiguous = find_jailed_descendant(stage2_pid, descendant_pid);

        stage2.kill().unwrap();
        stage2.wait().unwrap();

        let ambiguous = ambiguous.unwrap_err();
        assert!(
            ambiguous.to_string().contains("2 children"),
            "an ambiguous stage 2 must be reported as one, and counted: {ambiguous}"
        );
    }

    #[test]
    fn a_stage_1_that_exited_on_its_own_is_not_a_kill() {
        let executable = std::env::current_exe().unwrap();
        let mut killed = Command::new(&executable)
            .args([
                "--exact",
                "transition::tests::kill_hold_descendant_helper",
                "--nocapture",
            ])
            .env("TD_JAIL_TEST_KILL_HOLD_DESCENDANT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        killed.kill().unwrap();
        require_killed_by_signal(killed.wait().unwrap()).unwrap();

        // The same helper with its env var unset returns immediately and
        // exits 0, which is exactly the "was already leaving" shape: the
        // process is gone, and it is gone for a reason the probe did not
        // cause.
        let quiet = Command::new(&executable)
            .args([
                "--exact",
                "transition::tests::kill_hold_descendant_helper",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(quiet.status.success(), "the quiet helper must exit cleanly");
        assert!(require_killed_by_signal(quiet.status).is_err());
    }

    #[test]
    fn the_instance_shape_is_required_before_the_kill_can_mean_anything() {
        // Stage 2 is PID 1 of its own namespace; the descendant is inside
        // that namespace, so its innermost pid is not its host pid.
        let stage2 = cgroup::HostProcess {
            parent: 900,
            starttime: 10,
            state: 'S',
            namespace_pid: 1,
        };
        let descendant = cgroup::HostProcess {
            parent: 901,
            starttime: 11,
            state: 'S',
            namespace_pid: 2,
        };
        require_instance_shape(901, 902, stage2, descendant).unwrap();

        // A descendant of something other than stage 2 is not this
        // instance's, so killing stage 1 says nothing about it.
        let stray = cgroup::HostProcess {
            parent: 903,
            ..descendant
        };
        assert!(require_instance_shape(901, 902, stage2, stray).is_err());
        // A stage 2 that is not PID 1 never made a namespace, so nothing
        // below it would be torn down by its exit.
        let unnested = cgroup::HostProcess {
            namespace_pid: 901,
            ..stage2
        };
        assert!(require_instance_shape(901, 902, unnested, descendant).is_err());
        // A descendant whose namespace pid is its host pid is in the host
        // namespace: it would have to be killed by something, and this probe
        // kills only stage 1.
        let outside = cgroup::HostProcess {
            namespace_pid: 902,
            ..descendant
        };
        assert!(require_instance_shape(901, 902, stage2, outside).is_err());
        let init = cgroup::HostProcess {
            namespace_pid: 1,
            ..descendant
        };
        assert!(require_instance_shape(901, 902, stage2, init).is_err());
    }

    /// The false pass this probe would otherwise admit, pinned as
    /// arithmetic rather than as prose.
    ///
    /// Stage 2 and the descendant end THEMSELVES at `KILL_HOLD_CEILING`. If
    /// the driver could still be inside its polling window when that
    /// happens, both would leave `/proc` for a reason that has nothing to
    /// do with stage 1, and the probe would print its marker having proved
    /// nothing -- even with both liveness mechanisms broken.
    ///
    /// The watchdogs are what stop it: no run reaches the kill later than
    /// `KILL_REAPS_CEILING`, and the window is `KILL_REAPS_TIMEOUT` long.
    /// That is only safe while the sum stays under the holders' ceiling,
    /// which is a relationship between four constants that any one of them
    /// could be edited out of.
    #[test]
    fn the_holders_outlive_every_window_the_driver_can_still_be_watching_in() {
        assert!(
            KILL_REAPS_CEILING + KILL_REAPS_TIMEOUT < KILL_HOLD_CEILING,
            "a held role must not be able to end itself while the driver is still \
             watching for it to go: the driver gives up at {KILL_REAPS_CEILING:?} and \
             watches for {KILL_REAPS_TIMEOUT:?} after that, but the holders end at \
             {KILL_HOLD_CEILING:?}"
        );
    }

    #[test]
    fn a_witness_pid_below_two_is_refused_without_reading_proc() {
        // 0 is not a pid and 1 is the host's init, which is neither stage 2
        // nor anything this probe spawned. Reading either would be asking
        // /proc about the wrong process, so the floor comes first.
        for pid in [0, 1] {
            let refused = require_live("stage 2", pid).unwrap_err();
            assert!(
                refused.to_string().contains(&format!("reported {pid}")),
                "pid {pid} must be refused by name: {refused}"
            );
        }
        // And two is not refused for being small: the floor is a floor,
        // not a guess at which pids are real. Whether pid 2 exists here is
        // the host's business -- what matters is that the answer came from
        // looking, so the floor's own complaint must not be the one back.
        if let Err(error) = require_live("stage 2", 2) {
            assert!(
                !error.to_string().contains("reported 2"),
                "pid 2 must be looked up, not refused out of hand: {error}"
            );
        }
    }

    #[test]
    fn an_already_dead_witness_is_refused_before_the_kill() {
        let live = cgroup::HostProcess {
            parent: 900,
            starttime: 10,
            state: 'S',
            namespace_pid: 1,
        };
        let descendant = cgroup::HostProcess {
            parent: 901,
            starttime: 11,
            state: 'S',
            namespace_pid: 2,
        };
        require_instance_shape(901, 902, live, descendant).unwrap();
        // Either witness already exited is a refusal, because the whole
        // measurement is "these two were here, then the kill, then they
        // were not".
        let dead = cgroup::HostProcess { state: 'Z', ..live };
        assert!(require_instance_shape(901, 902, dead, descendant).is_err());
        let dead = cgroup::HostProcess {
            state: 'Z',
            ..descendant
        };
        assert!(require_instance_shape(901, 902, live, dead).is_err());
    }

    #[test]
    fn a_reused_pid_reads_as_gone_rather_than_as_a_survivor() {
        let live = std::process::id();
        let starttime = cgroup::host_process(live).unwrap().unwrap().starttime;
        assert!(!is_gone(live, starttime).unwrap());
        // The same pid with any other start time is a different process, so
        // whatever was being measured is gone. Without this the driver would
        // wait out its whole deadline on a busy host and blame the teardown.
        assert!(is_gone(live, starttime.wrapping_add(1)).unwrap());
    }

    #[test]
    fn a_zombie_reads_as_gone_rather_than_as_a_survivor() {
        // A real one: spawn, let it exit, and never wait for it. Its
        // `/proc` entry survives with its original start time, which is
        // exactly the shape an orphaned stage 2 has between exiting and
        // being adopted -- and reading it as live would fail the probe for
        // someone else's scheduling rather than for a teardown that broke.
        let executable = std::env::current_exe().unwrap();
        let mut corpse = Command::new(executable)
            .args([
                "--exact",
                "transition::tests::kill_hold_descendant_helper",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = corpse.id();
        let starttime = cgroup::host_process(pid).unwrap().unwrap().starttime;

        let deadline = Instant::now() + Duration::from_secs(10);
        let zombie = loop {
            let process = cgroup::host_process(pid).unwrap();
            match process {
                Some(process) if process.state == 'Z' => break true,
                // Reaped by something else before it was observed: the
                // teardown reading is the same, so the assertion below
                // still holds and the test is not weakened.
                None => break false,
                Some(_) if Instant::now() >= deadline => {
                    panic!("the helper never exited, so no zombie was produced")
                }
                Some(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        assert!(
            is_gone(pid, starttime).unwrap(),
            "a {} process must read as gone",
            if zombie { "zombie" } else { "reaped" }
        );
        corpse.wait().unwrap();
    }

    /// The report is a HOST pid and a NAMESPACE pid, in that order.
    ///
    /// `"41 1"` is the interesting refusal: pid 1 of the jail is stage 2, so
    /// a descendant reported as 1 is stage 2 named a second time, and the
    /// descendant exists precisely to be a different process. The two pids
    /// being EQUAL is not refused any more and must not be -- a host pid and
    /// a namespace pid share no numbering, so `"41 41"` is an ordinary run
    /// in which the jail's second process happens to be numbered like stage
    /// 2's host pid.
    #[test]
    fn the_kill_reaps_report_admits_a_host_pid_and_a_namespace_pid() {
        assert_eq!(parse_kill_reaps_report("41 42").unwrap(), (41, 42));
        assert_eq!(parse_kill_reaps_report("41 41").unwrap(), (41, 41));
        for refused in ["41", "41 42 43", "41 1", "41 0", "41  42", " 41 42", "41 x", ""] {
            assert!(
                parse_kill_reaps_report(refused).is_err(),
                "{refused:?} must not parse as a kill-reaps report"
            );
        }
    }

    #[test]
    fn the_held_stage_2_is_read_by_its_exact_marker() {
        let line = format!("{STAGE2_HOLD_MARKER} descendant=2");
        assert_eq!(parse_stage2_hold(&line).unwrap(), 2);
        for refused in [
            "TD-JAIL-STAGE2-OK descendant=2",
            "TD-JAIL-STAGE2-HOLDING descendant=",
            "TD-JAIL-STAGE2-HOLDING descendant=-1",
            "TD-JAIL-STAGE2-HOLDING 2",
            "descendant=2",
            "",
        ] {
            assert!(
                parse_stage2_hold(refused).is_err(),
                "{refused:?} must not read as a stage-2 hold report"
            );
        }
    }

    #[test]
    fn a_report_without_its_terminator_is_refused_rather_than_used() {
        let mut whole = "41 42\n".as_bytes();
        assert_eq!(
            read_report_line(&mut whole, "role").unwrap(),
            "41 42".to_string()
        );
        // EOF mid-line is the shape a role that died before reporting
        // produces, and reading it as a complete report would name pids it
        // never finished writing.
        let mut truncated = "41 4".as_bytes();
        assert!(read_report_line(&mut truncated, "role").is_err());
        let long = format!("{}\n", "9".repeat(KILL_REAPS_REPORT_LIMIT as usize + 1));
        let mut long = long.as_bytes();
        assert!(read_report_line(&mut long, "role").is_err());
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
                // ESRCH as well as ENOENT: a task being reaped between the
                // open and the read gives `ESRCH`, which `std` leaves as
                // `Uncategorized`, and this loop polls exactly while that
                // is happening. Seen failing here as `No such process (os
                // error 3)`. `cgroup::reads_as_gone` says the same thing
                // for the production readers.
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(3) =>
                {
                    break
                }
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
                &mount_identity_for_path(mountinfo, allowed_home).unwrap(),
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
            &mount_identity_for_path(&reserved_alias, allowed_home).unwrap(),
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
    fn grant_mount_identity_allows_home_below_reserved_backing_volume() {
        let base = "1 0 7:0 / / ro - erofs root ro\n\
                    2 1 0:16 / /run rw - tmpfs run rw\n\
                    3 2 0:18 / /run/td-volume ro - btrfs volume ro\n\
                    4 1 0:18 /@var /var rw - btrfs volume rw\n\
                    5 4 0:18 /@var/home/tester/Downloads /var/home/tester/Downloads rw - btrfs volume rw\n";
        let source = Path::new("/var/home/tester/Downloads");
        let allowed_home = Path::new("/var/home/tester");
        let home_roots = vec![PathBuf::from("/var/home")];
        let state_root = Path::new("/var/home/tester/.td/app");

        let check = |mountinfo: &str| {
            let mut reserved = mount_tree_identities(mountinfo, Path::new("/run")).unwrap();
            reserved.extend(mount_tree_identities(mountinfo, state_root).unwrap());
            require_grant_mount_identities(
                source,
                &mount_tree_identities(mountinfo, source).unwrap(),
                &reserved,
                &mount_identity_for_path(mountinfo, allowed_home).unwrap(),
                &mount_tree_identities(mountinfo, Path::new("/var/home")).unwrap(),
                &mount_tree_identities(mountinfo, allowed_home).unwrap(),
                &mount_identities_outside_allowed_home(
                    mountinfo,
                    &home_roots,
                    allowed_home,
                )
                .unwrap(),
            )
        };

        check(base).unwrap();

        let own_home_nested = format!(
            "{base}6 5 0:18 /@var/home/tester/Documents \
             /var/home/tester/Downloads/nested rw - btrfs volume rw\n"
        );
        check(&own_home_nested).unwrap();

        let other_home_nested = format!(
            "{base}6 4 0:18 /@var/home/other /var/home/other rw - btrfs volume rw\n\
             7 5 0:18 /@var/home/other \
             /var/home/tester/Downloads/nested rw - btrfs volume rw\n"
        );
        assert!(check(&other_home_nested)
            .unwrap_err()
            .to_string()
            .contains("aliases a reserved mount"));
        assert!(require_grant_mount_identities(
            source,
            &mount_tree_identities(&other_home_nested, source).unwrap(),
            &mount_tree_identities(&other_home_nested, state_root).unwrap(),
            &mount_identity_for_path(&other_home_nested, allowed_home).unwrap(),
            &mount_tree_identities(&other_home_nested, Path::new("/var/home")).unwrap(),
            &mount_tree_identities(&other_home_nested, allowed_home).unwrap(),
            &mount_identities_outside_allowed_home(
                &other_home_nested,
                &home_roots,
                allowed_home,
            )
            .unwrap(),
        )
        .unwrap_err()
        .to_string()
        .contains("aliases another home mount"));

        let whole_volume_nested =
            format!("{base}6 5 0:18 / /var/home/tester/Downloads/nested rw - btrfs volume rw\n");
        assert!(check(&whole_volume_nested)
            .unwrap_err()
            .to_string()
            .contains("aliases a reserved mount"));

        let state_alias = base.replace(
            "/@var/home/tester/Downloads /var/home/tester/Downloads",
            "/@var/home/tester/.td/app /var/home/tester/Downloads",
        );
        assert!(check(&state_alias)
            .unwrap_err()
            .to_string()
            .contains("aliases a reserved mount"));
    }

    #[test]
    fn grant_mount_identity_keeps_backing_volume_exception_home_specific() {
        let mountinfo = "1 0 7:0 / / ro - erofs root ro\n\
                         2 1 0:16 / /run rw - tmpfs run rw\n\
                         3 2 0:18 / /run/td-volume ro - btrfs volume ro\n\
                         4 1 0:18 /@var /var rw - btrfs volume rw\n";
        let source = Path::new("/var/media");
        let allowed_home = Path::new("/var/home/tester");
        assert!(require_grant_mount_identities(
            source,
            &mount_tree_identities(mountinfo, source).unwrap(),
            &mount_tree_identities(mountinfo, Path::new("/run")).unwrap(),
            &mount_identity_for_path(mountinfo, allowed_home).unwrap(),
            &mount_tree_identities(mountinfo, Path::new("/var/home")).unwrap(),
            &mount_tree_identities(mountinfo, allowed_home).unwrap(),
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string()
        .contains("aliases a reserved mount"));

        let root_home = "1 0 7:0 / / ro - erofs root ro\n\
                         2 1 0:16 / /run rw - tmpfs run rw\n\
                         3 2 0:19 / /run/td-volume ro - ext4 home ro\n\
                         4 1 0:19 / /var/home/tester rw - ext4 home rw\n";
        assert!(require_grant_mount_identities(
            Path::new("/var/home/tester/Downloads"),
            &mount_tree_identities(root_home, Path::new("/var/home/tester/Downloads")).unwrap(),
            &mount_tree_identities(root_home, Path::new("/run")).unwrap(),
            &mount_identity_for_path(root_home, allowed_home).unwrap(),
            &mount_tree_identities(root_home, Path::new("/var/home")).unwrap(),
            &mount_tree_identities(root_home, allowed_home).unwrap(),
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string()
        .contains("aliases a reserved mount"));

        let whole_home_alias = "1 0 7:0 / / ro - erofs root ro\n\
                                2 1 0:16 / /run rw - tmpfs run rw\n\
                                3 2 0:18 / /run/td-volume ro - btrfs volume ro\n\
                                4 1 0:18 /@var /var rw - btrfs volume rw\n\
                                5 4 0:20 / /var/home/tester/.td/app rw - tmpfs state rw\n\
                                6 4 0:18 /@var/home/tester \
                                /var/home/tester/homelink rw - btrfs volume rw\n";
        let alias = Path::new("/var/home/tester/homelink");
        let mut reserved =
            mount_tree_identities(whole_home_alias, Path::new("/run")).unwrap();
        reserved.extend(
            mount_tree_identities(whole_home_alias, Path::new("/var/home/tester/.td/app"))
                .unwrap(),
        );
        let whole_home_error = require_grant_mount_identities(
            alias,
            &mount_tree_identities(whole_home_alias, alias).unwrap(),
            &reserved,
            &mount_identity_for_path(whole_home_alias, allowed_home).unwrap(),
            &mount_tree_identities(whole_home_alias, Path::new("/var/home")).unwrap(),
            &mount_tree_identities(whole_home_alias, allowed_home).unwrap(),
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(whole_home_error.contains(
            "identity 0:18:/@var/home/tester aliases a reserved mount identity 0:18:/"
        ));
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
        assert_eq!(
            RUNTIME_ALIASES,
            [
                ("bin", "/usr/bin"),
                ("lib", "/usr/lib"),
                ("lib64", "/usr/lib64"),
                ("sbin", "/usr/sbin"),
            ]
        );
        let base = grant_scaffold_names(true, false, &[]).unwrap();
        assert_eq!(
            base.get(Path::new("/")),
            Some(&BTreeSet::from([
                "app".to_string(),
                "dev".to_string(),
                "etc".to_string(),
                "home".to_string(),
                "proc".to_string(),
                "run".to_string(),
                "tmp".to_string(),
                "usr".to_string(),
                "var".to_string(),
            ]))
        );
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
        let dynamic = grant_scaffold_names(true, true, &[]).unwrap();
        for alias in ["bin", "lib", "lib64", "sbin"] {
            assert!(dynamic[Path::new("/")].contains(alias));
            assert!(!base[Path::new("/")].contains(alias));
        }
        let names = grant_scaffold_names(true, false, &filesystems).unwrap();
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

    /// The stage-2 readback answers "is this the runtime's own file for the
    /// zone the system named", and every way of getting that wrong.
    ///
    /// A hard link stands in for the bind: `stat` cannot tell one from the
    /// other, and `stat` is all this reads.
    #[test]
    fn the_bound_zone_must_be_the_runtimes_own_file_for_the_named_zone() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory().unwrap();
        // Canonical, as the shipped caller's `/usr` is: a `TMPDIR` with a
        // symlinked component would otherwise put every zone outside the
        // prefix and fail this for a reason that is not the rule.
        let root = fs::canonicalize(directory.as_path()).unwrap();
        let runtime = root.join("runtime");
        let zoneinfo = runtime.join(authority::ZONEINFO_SUBDIR);
        fs::create_dir_all(zoneinfo.join("Europe")).unwrap();
        fs::create_dir_all(zoneinfo.join("Etc")).unwrap();
        fs::write(zoneinfo.join("Europe/Berlin"), b"TZif2 Berlin").unwrap();
        fs::write(zoneinfo.join("Etc/UTC"), b"TZif2 UTC").unwrap();
        symlink("Etc/UTC", zoneinfo.join("UTC")).unwrap();
        fs::write(zoneinfo.join("Bogus"), b"not a compiled zone").unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"TZif2 outside").unwrap();
        symlink(&outside, zoneinfo.join("Escape")).unwrap();
        // A `share` that links to a sibling INSIDE the runtime is legal on
        // both sides — the pair of rules is the same pair stage 1 applies, so
        // a tree stage 1 accepts is a tree this accepts.
        let sibling_runtime = root.join("sibling");
        fs::create_dir(&sibling_runtime).unwrap();
        fs::create_dir_all(sibling_runtime.join("real/zoneinfo/Etc")).unwrap();
        fs::write(
            sibling_runtime.join("real/zoneinfo/Etc/UTC"),
            b"TZif2 sibling",
        )
        .unwrap();
        symlink("real", sibling_runtime.join("share")).unwrap();

        let localtime = root.join("localtime");
        let bind = |source: &Path| {
            let _ = fs::remove_file(&localtime);
            fs::hard_link(source, &localtime).unwrap();
        };

        // The named zone's own file, reached directly and through a link.
        bind(&zoneinfo.join("Europe/Berlin"));
        require_bound_zone_at(&localtime, &runtime, "Europe/Berlin").unwrap();
        bind(&zoneinfo.join("Etc/UTC"));
        require_bound_zone_at(&localtime, &runtime, "UTC").unwrap();
        require_bound_zone_at(&localtime, &runtime, "Etc/UTC").unwrap();

        // A zone file, but not the one the system named.
        assert!(require_bound_zone_at(&localtime, &runtime, "Europe/Berlin").is_err());
        // A name outside the grammar never reaches the filesystem.
        assert!(require_bound_zone_at(&localtime, &runtime, "../outside").is_err());
        // A source that leaves the zoneinfo tree is refused HERE too, not
        // only by stage 1: containment proved on one side of the bind is
        // containment this readback is trusting rather than checking.
        bind(&outside);
        assert!(require_bound_zone_at(&localtime, &runtime, "Escape").is_err());
        // Bound bytes that are not a compiled zone: glibc would read them as
        // UTC in silence.
        bind(&zoneinfo.join("Bogus"));
        assert!(require_bound_zone_at(&localtime, &runtime, "Bogus").is_err());
        // Nothing bound at all, and a directory where the file should be.
        fs::remove_file(&localtime).unwrap();
        assert!(require_bound_zone_at(&localtime, &runtime, "UTC").is_err());
        fs::create_dir(&localtime).unwrap();
        assert!(require_bound_zone_at(&localtime, &runtime, "UTC").is_err());
        fs::remove_dir(&localtime).unwrap();

        let sibling_zone = sibling_runtime.join("real/zoneinfo/Etc/UTC");
        fs::hard_link(&sibling_zone, &localtime).unwrap();
        require_bound_zone_at(&localtime, &sibling_runtime, "Etc/UTC").unwrap();
    }

    #[test]
    fn selective_etc_identity_is_exact_and_runtime_entries_are_closed() {
        let directory = temporary_directory().unwrap();
        let runtime = directory.join("runtime");
        let etc = runtime.join("etc");
        fs::create_dir_all(etc.join("fonts")).unwrap();
        fs::write(etc.join("ld.so.conf"), b"include /etc/ld.so.conf.d/*.conf\n").unwrap();
        fs::write(etc.join("unknown"), b"not selected\n").unwrap();
        let fonts = fs::symlink_metadata(etc.join("fonts")).unwrap();
        let ld_so_conf = fs::symlink_metadata(etc.join("ld.so.conf")).unwrap();

        let selected = selected_runtime_etc(&runtime).unwrap();
        assert_eq!(
            selected,
            [
                RuntimeEtcEntry {
                    name: "fonts",
                    source: etc.join("fonts"),
                    source_kind: FilesystemSourceKind::Directory,
                    source_device: fonts.dev(),
                    source_inode: fonts.ino(),
                },
                RuntimeEtcEntry {
                    name: "ld.so.conf",
                    source: etc.join("ld.so.conf"),
                    source_kind: FilesystemSourceKind::File,
                    source_device: ld_so_conf.dev(),
                    source_inode: ld_so_conf.ino(),
                },
            ]
        );
        let binding = |timezone, firefox_autotest_policy| EtcBinding {
            resolv_conf: false,
            timezone,
            firefox_autotest_policy,
            machine_id: Some("0123456789abcdef0123456789abcdef\n"),
        };
        assert!(!expected_etc_names(&selected, binding(None, false))
            .unwrap()
            .contains("firefox"));
        // A launch with no zone expects no `localtime`, and one with a zone
        // expects exactly one more name — the entry is conditional the way
        // `resolv.conf` is, not unconditional the way `machine-id` is.
        assert!(!expected_etc_names(&selected, binding(None, true))
            .unwrap()
            .contains("localtime"));
        assert!(
            expected_etc_names(&selected, binding(Some("Europe/Berlin"), true))
                .unwrap()
                .contains("localtime")
        );
        assert_eq!(
            expected_etc_names(&selected, binding(None, true)).unwrap(),
            BTreeSet::from([
                "firefox".to_string(),
                "fonts".to_string(),
                "group".to_string(),
                "hostname".to_string(),
                "hosts".to_string(),
                "ld.so.conf".to_string(),
                "machine-id".to_string(),
                "nsswitch.conf".to_string(),
                "passwd".to_string(),
                "ssl".to_string(),
            ])
        );
        fs::remove_file(etc.join("ld.so.conf")).unwrap();
        fs::write(etc.join("ld.so.conf"), b"changed identity\n").unwrap();
        assert!(require_runtime_etc_source_identity(selected.get(1).unwrap()).is_err());
        symlink("fonts", etc.join("xdg")).unwrap();
        assert!(selected_runtime_etc(&runtime).is_err());
        fs::remove_dir_all(directory).unwrap();
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

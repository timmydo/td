//! The operator-owned application permission policy.
//!
//! `APPLICATIONS.md` section B.2 keeps this keyfile separate from immutable
//! package metadata and the compiled jail spec. This module gives that mutable
//! policy a closed, typed vocabulary; the later spec compiler consumes these
//! values rather than interpreting arbitrary keys at launch.

use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PERMISSION_FILE_BYTES: usize = 16 * 1024;
pub const MAX_FILESYSTEM_ENTRIES: usize = 128;
const MAX_BUS_POLICY_ENTRIES: usize = 128;

/// The most `own` entries a launch can actually deliver.
///
/// A permission file may carry `MAX_BUS_POLICY_ENTRIES` session-bus rows, and
/// that ceiling is about the FILE. This one is about what reaches the broker:
/// `td-busd`'s `MAX_OWNED_NAMES` is the same number, and it is 32 because the
/// broker copies an instance's grant into every identity it resolves, once
/// per accept. The two constants cannot be shared — `td-busd` is a standalone
/// dependency-free lock and does not link this crate — so they are stated
/// twice and each names the other. Changing one without the other turns a
/// clear refusal here into `InvalidArgs: that list of names cannot be read`
/// at launch, which is what happened before this rule existed.
pub const MAX_OWNED_BUS_NAMES: usize = 32;
const MAX_FILESYSTEM_LOCATION_BYTES: usize = 4096;
const MAX_BUS_NAME_BYTES: usize = 255;
pub const RESERVED_FILESYSTEM_TREES: &[&str] = &[
    "/app",
    "/usr",
    "/bin",
    "/run",
    "/proc",
    "/sys",
    "/dev",
    "/tmp",
    "/home",
    "/root",
    "/var/home",
    "/var/root",
    "/var/run",
    "/var/tmp",
    "/etc",
    "/boot",
    "/.flatpak-info",
    "/var/lib/td",
    "/var/lib/flatpak",
];
const PAGE_SIZE_BYTES: u64 = 4096;
// The largest aligned value below Linux x86-64's page-counter `max` sentinel.
const MAX_MEMORY_BYTES: u64 = (((i64::MAX as u64) / PAGE_SIZE_BYTES) - 1) * PAGE_SIZE_BYTES;
// Linux x86-64's PID_MAX_LIMIT, which bounds pids.max.
const MAX_PIDS: u32 = 4 * 1024 * 1024;
const MIN_CPU_BANDWIDTH_USEC: u64 = 1000;
const MAX_CPU_PERIOD_USEC: u64 = 1_000_000;
// Linux 7.1.4's MAX_BW with BW_SHIFT=20.
const MAX_CPU_QUOTA_USEC: u64 = (1_u64 << (64 - 20)) - 1;
pub const DEFAULT_MEMORY_HIGH_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_MEMORY_MAX_BYTES: u64 = 1280 * 1024 * 1024;
pub const DEFAULT_PIDS_MAX: u32 = 1024;
pub const DEFAULT_CPU_QUOTA_USEC: u64 = 100_000;
pub const DEFAULT_CPU_PERIOD_USEC: u64 = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionSocket {
    Wayland,
    PulseAudio,
}

impl PermissionSocket {
    fn parse(value: &str) -> Result<PermissionSocket, String> {
        match value {
            "wayland" => Ok(PermissionSocket::Wayland),
            "pulseaudio" => Ok(PermissionSocket::PulseAudio),
            _ => Err(format!("unknown application socket {value:?}")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            PermissionSocket::Wayland => "wayland",
            PermissionSocket::PulseAudio => "pulseaudio",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesystemAccess {
    Deny,
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilesystemPermission {
    mode: FilesystemMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilesystemMode {
    Deny,
    ReadOnly,
    ReadOnlyCreate,
    ReadWrite,
    ReadWriteCreate,
}

impl FilesystemPermission {
    fn new(
        location: &str,
        access: FilesystemAccess,
        create: bool,
    ) -> Result<FilesystemPermission, String> {
        let mode = match (access, create) {
            (FilesystemAccess::Deny, false) => FilesystemMode::Deny,
            (FilesystemAccess::Deny, true) => {
                return Err("a denied filesystem location cannot carry `create'".into());
            }
            (FilesystemAccess::ReadOnly, false) => FilesystemMode::ReadOnly,
            (FilesystemAccess::ReadOnly, true) => FilesystemMode::ReadOnlyCreate,
            (FilesystemAccess::ReadWrite, false) => FilesystemMode::ReadWrite,
            (FilesystemAccess::ReadWrite, true) => FilesystemMode::ReadWriteCreate,
        };
        FilesystemPermission::from_mode(location, mode)
    }

    fn parse(location: &str, value: &str) -> Result<FilesystemPermission, String> {
        let mode = match value {
            "deny" => FilesystemMode::Deny,
            "ro" => FilesystemMode::ReadOnly,
            "rw" => FilesystemMode::ReadWrite,
            "ro:create" => FilesystemMode::ReadOnlyCreate,
            "rw:create" => FilesystemMode::ReadWriteCreate,
            _ => {
                return Err(format!(
                    "filesystem permission must be `deny', `ro', `rw', `ro:create' or `rw:create', not {value:?}"
                ));
            }
        };
        FilesystemPermission::from_mode(location, mode)
    }

    fn from_mode(location: &str, mode: FilesystemMode) -> Result<FilesystemPermission, String> {
        validate_filesystem_location(
            location,
            matches!(
                mode,
                FilesystemMode::ReadOnlyCreate | FilesystemMode::ReadWriteCreate
            ),
        )?;
        Ok(FilesystemPermission { mode })
    }

    fn as_str(self) -> &'static str {
        match self.mode {
            FilesystemMode::Deny => "deny",
            FilesystemMode::ReadOnly => "ro",
            FilesystemMode::ReadOnlyCreate => "ro:create",
            FilesystemMode::ReadWrite => "rw",
            FilesystemMode::ReadWriteCreate => "rw:create",
        }
    }

    pub fn access(self) -> FilesystemAccess {
        match self.mode {
            FilesystemMode::Deny => FilesystemAccess::Deny,
            FilesystemMode::ReadOnly | FilesystemMode::ReadOnlyCreate => FilesystemAccess::ReadOnly,
            FilesystemMode::ReadWrite | FilesystemMode::ReadWriteCreate => {
                FilesystemAccess::ReadWrite
            }
        }
    }

    pub fn create(self) -> bool {
        matches!(
            self.mode,
            FilesystemMode::ReadOnlyCreate | FilesystemMode::ReadWriteCreate
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusAccess {
    See,
    Talk,
    Own,
}

impl BusAccess {
    fn parse(value: &str) -> Result<BusAccess, String> {
        match value {
            "see" => Ok(BusAccess::See),
            "talk" => Ok(BusAccess::Talk),
            "own" => Ok(BusAccess::Own),
            _ => Err(format!(
                "session bus policy must be `see', `talk' or `own', not {value:?}"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            BusAccess::See => "see",
            BusAccess::Talk => "talk",
            BusAccess::Own => "own",
        }
    }

    pub fn allows(self, required: BusAccess) -> bool {
        match self {
            BusAccess::See => required == BusAccess::See,
            BusAccess::Talk => required != BusAccess::Own,
            BusAccess::Own => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuMax {
    quota_usec: u64,
    period_usec: u64,
}

impl CpuMax {
    fn new(quota_usec: u64, period_usec: u64) -> Result<CpuMax, String> {
        validate_cpu_max(quota_usec, period_usec)?;
        Ok(CpuMax {
            quota_usec,
            period_usec,
        })
    }

    pub fn quota_usec(self) -> u64 {
        self.quota_usec
    }

    pub fn period_usec(self) -> u64 {
        self.period_usec
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    memory_high_bytes: Option<u64>,
    memory_max_bytes: Option<u64>,
    pids_max: Option<u32>,
    cpu_max: Option<CpuMax>,
}

impl ResourceLimits {
    pub fn memory_high_bytes(self) -> Option<u64> {
        self.memory_high_bytes
    }

    pub fn memory_max_bytes(self) -> Option<u64> {
        self.memory_max_bytes
    }

    pub fn pids_max(self) -> Option<u32> {
        self.pids_max
    }

    pub fn cpu_max(self) -> Option<CpuMax> {
        self.cpu_max
    }

    /// The launch-time baseline: omission means the reviewed defaults, never
    /// an unlimited cgroup. An explicit resource policy is atomic so a partial
    /// edit cannot silently mix operator intent with unrelated defaults.
    pub fn complete_or_default(self) -> Result<ResourceLimits, String> {
        let ResourceLimits {
            memory_high_bytes,
            memory_max_bytes,
            pids_max,
            cpu_max,
        } = self;
        let completed = match (memory_high_bytes, memory_max_bytes, pids_max, cpu_max) {
            (None, None, None, None) => ResourceLimits {
                memory_high_bytes: Some(DEFAULT_MEMORY_HIGH_BYTES),
                memory_max_bytes: Some(DEFAULT_MEMORY_MAX_BYTES),
                pids_max: Some(DEFAULT_PIDS_MAX),
                cpu_max: Some(CpuMax {
                    quota_usec: DEFAULT_CPU_QUOTA_USEC,
                    period_usec: DEFAULT_CPU_PERIOD_USEC,
                }),
            },
            (Some(memory_high_bytes), Some(memory_max_bytes), Some(pids_max), cpu_max) => ResourceLimits {
                memory_high_bytes: Some(memory_high_bytes),
                memory_max_bytes: Some(memory_max_bytes),
                pids_max: Some(pids_max),
                cpu_max: Some(cpu_max.unwrap_or(CpuMax {
                    quota_usec: DEFAULT_CPU_QUOTA_USEC,
                    period_usec: DEFAULT_CPU_PERIOD_USEC,
                })),
            },
            _ => {
                return Err(
                    "an explicit resource policy must set memory-high, memory-max and pids-max together; cpu-max cannot appear alone"
                        .into(),
                )
            }
        };
        completed.validate()?;
        for (name, value) in [
            ("memory-high", completed.memory_high_bytes),
            ("memory-max", completed.memory_max_bytes),
        ] {
            if value.is_some_and(|bytes| !bytes.is_multiple_of(PAGE_SIZE_BYTES)) {
                return Err(format!(
                    "{name} must be aligned to the {PAGE_SIZE_BYTES}-byte target page size"
                ));
            }
        }
        Ok(completed)
    }

    fn is_empty(self) -> bool {
        self.memory_high_bytes.is_none()
            && self.memory_max_bytes.is_none()
            && self.pids_max.is_none()
            && self.cpu_max.is_none()
    }

    fn validate(self) -> Result<(), String> {
        if let (Some(high), Some(max)) = (self.memory_high_bytes, self.memory_max_bytes) {
            if high >= max {
                return Err(format!(
                    "memory-high must be below memory-max, not {high} >= {max}"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionFormat {
    One,
    Two,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionPolicy {
    network: bool,
    sockets: BTreeSet<PermissionSocket>,
    allow_devel: bool,
    filesystems: BTreeMap<String, FilesystemPermission>,
    session_bus: BTreeMap<String, BusAccess>,
    resources: ResourceLimits,
}

impl PermissionPolicy {
    pub fn new() -> PermissionPolicy {
        PermissionPolicy::default()
    }

    pub fn parse(text: &str) -> Result<PermissionPolicy, String> {
        parse_permission_policy(text)
    }

    pub fn with_network(mut self) -> Result<PermissionPolicy, String> {
        if self.network {
            return Err("duplicate shared network permission".into());
        }
        self.network = true;
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_socket(mut self, socket: PermissionSocket) -> Result<PermissionPolicy, String> {
        if !self.sockets.insert(socket) {
            return Err(format!(
                "duplicate application socket {:?}",
                socket.as_str()
            ));
        }
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_development(mut self) -> Result<PermissionPolicy, String> {
        if self.allow_devel {
            return Err("duplicate allow-devel feature".into());
        }
        self.allow_devel = true;
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_filesystem(
        mut self,
        location: &str,
        access: FilesystemAccess,
        create: bool,
    ) -> Result<PermissionPolicy, String> {
        if self.filesystems.contains_key(location) {
            return Err(format!("duplicate filesystem location {location:?}"));
        }
        if self.filesystems.len() >= MAX_FILESYSTEM_ENTRIES {
            return Err(format!(
                "a permission file may carry at most {MAX_FILESYSTEM_ENTRIES} filesystem entries"
            ));
        }
        let permission = FilesystemPermission::new(location, access, create)?;
        self.filesystems.insert(location.to_string(), permission);
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_session_bus(
        mut self,
        name: &str,
        access: BusAccess,
    ) -> Result<PermissionPolicy, String> {
        validate_bus_name(name)?;
        validate_bus_access(name, access)?;
        if self.session_bus.contains_key(name) {
            return Err(format!("duplicate session bus name {name:?}"));
        }
        if self.session_bus.len() >= MAX_BUS_POLICY_ENTRIES {
            return Err(format!(
                "a permission file may carry at most {MAX_BUS_POLICY_ENTRIES} session bus entries"
            ));
        }
        self.session_bus.insert(name.to_string(), access);
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_memory_high(mut self, bytes: u64) -> Result<PermissionPolicy, String> {
        if self.resources.memory_high_bytes.is_some() {
            return Err("duplicate memory-high resource limit".into());
        }
        validate_memory("memory-high", bytes)?;
        self.resources.memory_high_bytes = Some(bytes);
        self.resources.validate()?;
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_memory_max(mut self, bytes: u64) -> Result<PermissionPolicy, String> {
        if self.resources.memory_max_bytes.is_some() {
            return Err("duplicate memory-max resource limit".into());
        }
        validate_memory("memory-max", bytes)?;
        self.resources.memory_max_bytes = Some(bytes);
        self.resources.validate()?;
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_pids_max(mut self, pids: u32) -> Result<PermissionPolicy, String> {
        if self.resources.pids_max.is_some() {
            return Err("duplicate pids-max resource limit".into());
        }
        validate_pids(pids)?;
        self.resources.pids_max = Some(pids);
        self.ensure_size()?;
        Ok(self)
    }

    pub fn with_cpu_max(
        mut self,
        quota_usec: u64,
        period_usec: u64,
    ) -> Result<PermissionPolicy, String> {
        if self.resources.cpu_max.is_some() {
            return Err("duplicate cpu-max resource limit".into());
        }
        self.resources.cpu_max = Some(CpuMax::new(quota_usec, period_usec)?);
        self.ensure_size()?;
        Ok(self)
    }

    pub fn network(&self) -> bool {
        self.network
    }

    pub fn sockets(&self) -> impl Iterator<Item = PermissionSocket> + '_ {
        self.sockets.iter().copied()
    }

    pub fn allow_devel(&self) -> bool {
        self.allow_devel
    }

    pub fn filesystems(&self) -> impl Iterator<Item = (&str, FilesystemPermission)> {
        self.filesystems
            .iter()
            .map(|(location, permission)| (location.as_str(), *permission))
    }

    pub fn session_bus(&self) -> impl Iterator<Item = (&str, BusAccess)> {
        self.session_bus
            .iter()
            .map(|(name, access)| (name.as_str(), *access))
    }

    pub fn resources(&self) -> ResourceLimits {
        self.resources
    }

    /// The first thing this policy asks for that `td-jail` cannot honour yet.
    ///
    /// A named reason rather than a boolean, because the refusal reaches an
    /// operator holding a permission file and "policy not implemented" does
    /// not say which line to change.
    ///
    /// What is honoured is shared network, the wayland socket, filesystem
    /// entries, resource limits, and `[Session Bus Policy]` `own` entries —
    /// the last of which the broker consults when the application asks for
    /// the name. `see` and `talk` parse and are refused here: widening what a
    /// sandbox may ADDRESS is a decision about the imported services §B.3.2
    /// lists, not a mechanism this file is waiting on, and admitting the
    /// entries before that decision would make the file claim a grant nothing
    /// applies.
    ///
    /// Wayland is REQUIRED and not merely honoured, which is what the
    /// destructure below is for: every rule here is a statement about a named
    /// field, so a field added to this struct fails to compile rather than
    /// being silently admitted.
    pub fn unhonoured_request(&self) -> Option<String> {
        let PermissionPolicy {
            network: _,
            sockets,
            allow_devel,
            filesystems: _,
            session_bus,
            resources: _,
        } = self;
        if !sockets.contains(&PermissionSocket::Wayland) {
            return Some("a launch with no wayland socket".to_string());
        }
        if let Some(socket) = sockets
            .iter()
            .find(|socket| **socket != PermissionSocket::Wayland)
        {
            return Some(format!("the {} socket", socket.as_str()));
        }
        if *allow_devel {
            return Some("features=allow-devel".to_string());
        }
        if let Some((name, access)) = session_bus
            .iter()
            .find(|(_, access)| **access != BusAccess::Own)
        {
            return Some(format!("`{}' access to {name}", access.as_str()));
        }
        let owned = session_bus
            .values()
            .filter(|access| **access == BusAccess::Own)
            .count();
        if owned > MAX_OWNED_BUS_NAMES {
            return Some(format!(
                "{owned} `own' entries, more than the {MAX_OWNED_BUS_NAMES} a \
                 broker will record"
            ));
        }
        None
    }

    /// Canonical bytes for either immutable defaults or an operator override.
    pub fn to_keyfile(&self) -> String {
        let format = if self.resources.cpu_max.is_some() {
            "2"
        } else {
            "1"
        };
        let mut out = format!("format={format}\n");
        if self.network || !self.sockets.is_empty() || self.allow_devel {
            out.push_str("\n[Context]\n");
            if self.network {
                out.push_str("shared=network\n");
            }
            if !self.sockets.is_empty() {
                out.push_str("sockets=");
                push_list(&mut out, self.sockets.iter().map(|socket| socket.as_str()));
                out.push('\n');
            }
            if self.allow_devel {
                out.push_str("features=allow-devel\n");
            }
        }
        if !self.filesystems.is_empty() {
            out.push_str("\n[Filesystem]\n");
            for (location, permission) in &self.filesystems {
                push_key(&mut out, location, permission.as_str());
            }
        }
        if !self.session_bus.is_empty() {
            out.push_str("\n[Session Bus Policy]\n");
            for (name, access) in &self.session_bus {
                push_key(&mut out, name, access.as_str());
            }
        }
        if !self.resources.is_empty() {
            out.push_str("\n[Resources]\n");
            if let Some(value) = self.resources.memory_high_bytes {
                push_key(&mut out, "memory-high", &value.to_string());
            }
            if let Some(value) = self.resources.memory_max_bytes {
                push_key(&mut out, "memory-max", &value.to_string());
            }
            if let Some(value) = self.resources.pids_max {
                push_key(&mut out, "pids-max", &value.to_string());
            }
            if let Some(value) = self.resources.cpu_max {
                push_key(
                    &mut out,
                    "cpu-max",
                    &format!("{} {}", value.quota_usec(), value.period_usec()),
                );
            }
        }
        out
    }

    fn ensure_size(&self) -> Result<(), String> {
        let size = self.to_keyfile().len();
        if size > MAX_PERMISSION_FILE_BYTES {
            return Err(format!(
                "application permission file would be {size} bytes; the limit is {MAX_PERMISSION_FILE_BYTES}"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Context,
    Filesystem,
    SessionBus,
    Resources,
}

#[derive(Default)]
struct ParseState {
    policy: PermissionPolicy,
    format: Option<PermissionFormat>,
    sections: BTreeSet<Section>,
    shared_seen: bool,
    sockets_seen: bool,
    features_seen: bool,
}

fn parse_permission_policy(text: &str) -> Result<PermissionPolicy, String> {
    validate_file_shape(text)?;
    let mut state = ParseState::default();
    let mut section = None;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = trim_layout(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let parsed = parse_section(line).map_err(|reason| permission_line(number, &reason))?;
            if !state.sections.insert(parsed) {
                return Err(permission_line(
                    number,
                    &format!("duplicate {line} section"),
                ));
            }
            section = Some(parsed);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(permission_line(number, "expected key=value or a section"));
        };
        let key = trim_layout(key);
        let value = trim_layout(value);
        if key.is_empty() {
            return Err(permission_line(number, "empty key"));
        }
        if value.is_empty() {
            return Err(permission_line(number, &format!("empty value for {key:?}")));
        }
        let result = match section {
            None => apply_root(&mut state, key, value),
            Some(Section::Context) => apply_context(&mut state, key, value),
            Some(Section::Filesystem) => apply_filesystem(&mut state, key, value),
            Some(Section::SessionBus) => apply_session_bus(&mut state, key, value),
            Some(Section::Resources) => apply_resource(&mut state, key, value),
        };
        result.map_err(|reason| permission_line(number, &reason))?;
    }
    let format = state
        .format
        .ok_or_else(|| "application permission file is missing `format'".to_string())?;
    match (format, state.policy.resources.cpu_max) {
        (PermissionFormat::One, Some(_)) => {
            return Err("cpu-max requires application permission format=2".into());
        }
        (PermissionFormat::Two, None) => {
            return Err("application permission format=2 requires cpu-max".into());
        }
        _ => {}
    }
    state.policy.ensure_size()?;
    Ok(state.policy)
}

fn validate_file_shape(text: &str) -> Result<(), String> {
    if text.len() > MAX_PERMISSION_FILE_BYTES {
        return Err(format!(
            "application permission file is {} bytes; the limit is {MAX_PERMISSION_FILE_BYTES}",
            text.len()
        ));
    }
    if text.is_empty() {
        return Err("application permission file is empty".into());
    }
    if !text.ends_with('\n') {
        return Err("application permission file lacks a trailing newline".into());
    }
    if text.contains('\r') {
        return Err("application permission file contains a carriage return".into());
    }
    if text.contains('\0') {
        return Err("application permission file contains a NUL byte".into());
    }
    Ok(())
}

fn parse_section(line: &str) -> Result<Section, String> {
    match line {
        "[Context]" => Ok(Section::Context),
        "[Filesystem]" => Ok(Section::Filesystem),
        "[Session Bus Policy]" => Ok(Section::SessionBus),
        "[Resources]" => Ok(Section::Resources),
        _ => Err(format!("unknown permission section {line:?}")),
    }
}

fn apply_root(state: &mut ParseState, key: &str, value: &str) -> Result<(), String> {
    if key != "format" {
        return Err(format!("unknown permission root key {key:?}"));
    }
    if state.format.is_some() {
        return Err("duplicate permission key `format'".into());
    }
    state.format = Some(match value {
        "1" => PermissionFormat::One,
        "2" => PermissionFormat::Two,
        _ => {
            return Err(format!(
                "unsupported application permission format {value:?}; expected `1' or `2'"
            ));
        }
    });
    Ok(())
}

fn apply_context(state: &mut ParseState, key: &str, value: &str) -> Result<(), String> {
    match key {
        "shared" => {
            mark_once(&mut state.shared_seen, "shared")?;
            let values = parse_list("shared", value)?;
            if values.as_slice() != ["network"] {
                return Err(
                    "shared permissions contain an unsupported value; only `network' is admitted"
                        .into(),
                );
            }
            state.policy.network = true;
        }
        "sockets" => {
            mark_once(&mut state.sockets_seen, "sockets")?;
            for value in parse_list("sockets", value)? {
                let socket = PermissionSocket::parse(value)?;
                state.policy.sockets.insert(socket);
            }
        }
        "features" => {
            mark_once(&mut state.features_seen, "features")?;
            let values = parse_list("features", value)?;
            if values.as_slice() != ["allow-devel"] {
                return Err(
                    "features contain an unsupported value; only `allow-devel' is admitted".into(),
                );
            }
            state.policy.allow_devel = true;
        }
        "devices" => {
            let devices = parse_list("devices", value)?;
            for device in &devices {
                if !matches!(*device, "dri" | "tty") {
                    return Err(format!("unknown application device {device:?}"));
                }
            }
            let unavailable = if devices.contains(&"dri") {
                "devices=dri is recognized but unavailable until the hardware-rendering policy lands"
            } else {
                "devices=tty is unavailable until the fresh-terminal acquisition policy lands"
            };
            return Err(unavailable.into());
        }
        _ => return Err(format!("unknown [Context] key {key:?}")),
    }
    Ok(())
}

fn apply_filesystem(state: &mut ParseState, key: &str, value: &str) -> Result<(), String> {
    if state.policy.filesystems.contains_key(key) {
        return Err(format!("duplicate filesystem location {key:?}"));
    }
    if state.policy.filesystems.len() >= MAX_FILESYSTEM_ENTRIES {
        return Err(format!(
            "a permission file may carry at most {MAX_FILESYSTEM_ENTRIES} filesystem entries"
        ));
    }
    let permission = FilesystemPermission::parse(key, value)?;
    state.policy.filesystems.insert(key.to_string(), permission);
    Ok(())
}

fn apply_session_bus(state: &mut ParseState, key: &str, value: &str) -> Result<(), String> {
    validate_bus_name(key)?;
    if state.policy.session_bus.contains_key(key) {
        return Err(format!("duplicate session bus name {key:?}"));
    }
    if state.policy.session_bus.len() >= MAX_BUS_POLICY_ENTRIES {
        return Err(format!(
            "a permission file may carry at most {MAX_BUS_POLICY_ENTRIES} session bus entries"
        ));
    }
    let access = BusAccess::parse(value)?;
    validate_bus_access(key, access)?;
    state.policy.session_bus.insert(key.to_string(), access);
    Ok(())
}

fn apply_resource(state: &mut ParseState, key: &str, value: &str) -> Result<(), String> {
    match key {
        "memory-high" => {
            if state.policy.resources.memory_high_bytes.is_some() {
                return Err("duplicate resource key `memory-high'".into());
            }
            let bytes = parse_bounded_positive_u64(
                "memory-high",
                value,
                MAX_MEMORY_BYTES,
                "the maximum admitted byte count",
            )?;
            state.policy.resources.memory_high_bytes = Some(bytes);
        }
        "memory-max" => {
            if state.policy.resources.memory_max_bytes.is_some() {
                return Err("duplicate resource key `memory-max'".into());
            }
            let bytes = parse_bounded_positive_u64(
                "memory-max",
                value,
                MAX_MEMORY_BYTES,
                "the maximum admitted byte count",
            )?;
            state.policy.resources.memory_max_bytes = Some(bytes);
        }
        "pids-max" => {
            if state.policy.resources.pids_max.is_some() {
                return Err("duplicate resource key `pids-max'".into());
            }
            let pids = parse_pids(value)?;
            state.policy.resources.pids_max = Some(pids);
        }
        "cpu-max" => {
            if state.policy.resources.cpu_max.is_some() {
                return Err("duplicate resource key `cpu-max'".into());
            }
            state.policy.resources.cpu_max = Some(parse_cpu_max(value)?);
        }
        _ => return Err(format!("unknown [Resources] key {key:?}")),
    }
    state.policy.resources.validate()
}

fn mark_once(seen: &mut bool, key: &str) -> Result<(), String> {
    if *seen {
        return Err(format!("duplicate [Context] key {key:?}"));
    }
    *seen = true;
    Ok(())
}

fn parse_list<'a>(label: &str, value: &'a str) -> Result<Vec<&'a str>, String> {
    let mut values = Vec::new();
    for raw in value.split(';') {
        let item = trim_layout(raw);
        if item.is_empty() {
            return Err(format!("{label} contains an empty list item"));
        }
        if values.contains(&item) {
            return Err(format!("{label} contains duplicate value {item:?}"));
        }
        values.push(item);
    }
    Ok(values)
}

fn validate_filesystem_location(location: &str, create: bool) -> Result<(), String> {
    validate_scalar(
        "filesystem location",
        location,
        MAX_FILESYSTEM_LOCATION_BYTES,
    )?;
    if location.contains('=') {
        return Err("filesystem location may not contain `='".into());
    }
    if matches!(location, "host" | "home" | "~") {
        return Err(format!(
            "blanket filesystem location {location:?} is not admitted"
        ));
    }
    if is_xdg_location(location) {
        return Ok(());
    }
    if let Some(relative) = location.strip_prefix("~/") {
        validate_path_components("filesystem home subpath", relative)?;
        if paths_overlap(relative, ".local/share/flatpak") {
            return Err("the per-user Flatpak repository may not be granted".into());
        }
        return Ok(());
    }
    if !location.starts_with('/') {
        return Err(format!(
            "filesystem location {location:?} must be an xdg name, a `~/' subpath or an absolute path"
        ));
    }
    if location == "/" {
        return Err("filesystem root `/` may not be granted".into());
    }
    if create {
        return Err(
            "filesystem `create' is admitted only for xdg locations and `~/' subpaths".into(),
        );
    }
    let relative = location
        .strip_prefix('/')
        .ok_or("filesystem location is not absolute")?;
    validate_path_components("absolute filesystem path", relative)?;
    for reserved in RESERVED_FILESYSTEM_TREES {
        if paths_overlap(location, reserved) {
            return Err(format!(
                "reserved filesystem tree {reserved:?} may not be granted"
            ));
        }
    }
    Ok(())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    path_is_same_or_child(left, right) || path_is_same_or_child(right, left)
}

fn path_is_same_or_child(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_xdg_location(location: &str) -> bool {
    matches!(
        location,
        "xdg-download"
            | "xdg-documents"
            | "xdg-pictures"
            | "xdg-music"
            | "xdg-videos"
            | "xdg-desktop"
    )
}

fn validate_path_components(label: &str, path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{label} is empty"));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(format!("{label} contains an empty component"));
        }
        if matches!(component, "." | "..") {
            return Err(format!("{label} contains a {component:?} component"));
        }
        if component.chars().any(char::is_control) {
            return Err(format!("{label} contains a control character"));
        }
    }
    Ok(())
}

fn validate_bus_name(name: &str) -> Result<(), String> {
    validate_scalar("session bus name", name, MAX_BUS_NAME_BYTES)?;
    let mut components = 0usize;
    for component in name.split('.') {
        components += 1;
        let mut bytes = component.bytes();
        let Some(first) = bytes.next() else {
            return Err("session bus name contains an empty component".into());
        };
        if !(first.is_ascii_alphabetic() || matches!(first, b'_' | b'-')) {
            return Err(format!(
                "session bus component {component:?} must begin with an ASCII letter, `_` or `-'"
            ));
        }
        if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            return Err(format!(
                "session bus component {component:?} has a character outside [A-Za-z0-9_-]"
            ));
        }
    }
    if components < 2 {
        return Err("session bus name must contain at least one `.'".into());
    }
    Ok(())
}

fn validate_bus_access(name: &str, access: BusAccess) -> Result<(), String> {
    if access == BusAccess::Own
        && (name == "org.freedesktop.DBus"
            || name == "org.freedesktop.portal"
            || name.starts_with("org.freedesktop.portal.")
            || name == "org.freedesktop.impl.portal"
            || name.starts_with("org.freedesktop.impl.portal."))
    {
        return Err(format!(
            "reserved session bus name {name:?} may not be owned by an application"
        ));
    }
    Ok(())
}

fn validate_memory(label: &str, bytes: u64) -> Result<(), String> {
    if bytes == 0 {
        return Err(format!("{label} must be greater than zero"));
    }
    if bytes > MAX_MEMORY_BYTES {
        return Err(format!(
            "{label} exceeds the maximum admitted byte count {MAX_MEMORY_BYTES}"
        ));
    }
    Ok(())
}

fn validate_pids(pids: u32) -> Result<(), String> {
    if pids == 0 {
        return Err("pids-max must be greater than zero".into());
    }
    if pids > MAX_PIDS {
        return Err(format!("pids-max exceeds the kernel task limit {MAX_PIDS}"));
    }
    Ok(())
}

fn validate_cpu_max(quota_usec: u64, period_usec: u64) -> Result<(), String> {
    if quota_usec < MIN_CPU_BANDWIDTH_USEC {
        return Err(format!(
            "cpu-max quota must be at least {MIN_CPU_BANDWIDTH_USEC} microseconds"
        ));
    }
    if quota_usec > MAX_CPU_QUOTA_USEC {
        return Err(format!(
            "cpu-max quota exceeds the pinned kernel limit {MAX_CPU_QUOTA_USEC} microseconds"
        ));
    }
    if period_usec < MIN_CPU_BANDWIDTH_USEC {
        return Err(format!(
            "cpu-max period must be at least {MIN_CPU_BANDWIDTH_USEC} microseconds"
        ));
    }
    if period_usec > MAX_CPU_PERIOD_USEC {
        return Err(format!(
            "cpu-max period exceeds the pinned kernel limit {MAX_CPU_PERIOD_USEC} microseconds"
        ));
    }
    Ok(())
}

fn parse_bounded_positive_u64(
    label: &str,
    value: &str,
    max: u64,
    limit: &str,
) -> Result<u64, String> {
    if value.starts_with('+') || (value.len() > 1 && value.starts_with('0')) {
        return Err(format!(
            "{label} must be a canonical positive decimal integer"
        ));
    }
    let number = match value.parse::<u64>() {
        Ok(number) => number,
        Err(_) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
            return Err(format!("{label} exceeds {limit} {max}"));
        }
        Err(_) => return Err(format!("{label} is not an unsigned decimal integer")),
    };
    if number == 0 {
        return Err(format!("{label} must be greater than zero"));
    }
    if number > max {
        return Err(format!("{label} exceeds {limit} {max}"));
    }
    Ok(number)
}

fn trim_layout(value: &str) -> &str {
    value.trim_matches(|character| matches!(character, ' ' | '\t'))
}

fn parse_pids(value: &str) -> Result<u32, String> {
    let number = parse_bounded_positive_u64(
        "pids-max",
        value,
        u64::from(MAX_PIDS),
        "the kernel task limit",
    )?;
    let pids = u32::try_from(number)
        .map_err(|_| format!("pids-max exceeds the kernel task limit {MAX_PIDS}"))?;
    validate_pids(pids)?;
    Ok(pids)
}

fn parse_cpu_max(value: &str) -> Result<CpuMax, String> {
    let Some((quota, period)) = value.split_once(' ') else {
        return Err("cpu-max requires a quota and period in microseconds".into());
    };
    if quota.is_empty() || period.is_empty() || quota.contains(' ') || period.contains(' ') {
        return Err("cpu-max quota and period must be separated by exactly one ASCII space".into());
    }
    let quota_usec = parse_bounded_positive_u64(
        "cpu-max quota",
        quota,
        MAX_CPU_QUOTA_USEC,
        "the pinned kernel limit",
    )?;
    let period_usec = parse_bounded_positive_u64(
        "cpu-max period",
        period,
        MAX_CPU_PERIOD_USEC,
        "the pinned kernel limit",
    )?;
    CpuMax::new(quota_usec, period_usec)
}

fn validate_scalar(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} is empty"));
    }
    if value.len() > max {
        return Err(format!(
            "{label} is {} bytes; the limit is {max}",
            value.len()
        ));
    }
    if value.trim() != value {
        return Err(format!("{label} may not begin or end with whitespace"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} may not contain a control character"));
    }
    Ok(())
}

fn push_key(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

fn push_list<'a>(out: &mut String, values: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for value in values {
        if !first {
            out.push(';');
        }
        first = false;
        out.push_str(value);
    }
}

fn permission_line(line: usize, reason: &str) -> String {
    format!("application permission file line {line}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "format=1\n\n[Context]\nshared=network\nsockets=wayland;pulseaudio\nfeatures=allow-devel\n\n[Filesystem]\n/mnt/archive=ro\nxdg-download=rw:create\nxdg-pictures=deny\n~/Projects=rw:create\n\n[Session Bus Policy]\norg.freedesktop.FileManager1=talk\norg.mozilla.firefox=own\n\n[Resources]\nmemory-high=1073741824\nmemory-max=1342177280\npids-max=1024\n";

    fn error(text: &str) -> String {
        PermissionPolicy::parse(text).unwrap_err()
    }

    #[test]
    fn permissions_round_trip_to_one_canonical_keyfile() {
        let parsed = PermissionPolicy::parse(VALID).unwrap();
        assert!(parsed.network());
        assert!(parsed.allow_devel());
        assert_eq!(
            parsed.sockets().collect::<Vec<_>>(),
            [PermissionSocket::Wayland, PermissionSocket::PulseAudio]
        );
        assert_eq!(parsed.filesystems().count(), 4);
        let filesystems = parsed.filesystems().collect::<BTreeMap<_, _>>();
        assert_eq!(
            filesystems.get("xdg-download").map(|value| value.access()),
            Some(FilesystemAccess::ReadWrite)
        );
        assert_eq!(
            filesystems.get("xdg-download").map(|value| value.create()),
            Some(true)
        );
        assert_eq!(
            filesystems.get("xdg-pictures").map(|value| value.access()),
            Some(FilesystemAccess::Deny)
        );
        assert_eq!(parsed.session_bus().count(), 2);
        assert_eq!(parsed.resources().memory_high_bytes(), Some(1_073_741_824));
        assert_eq!(parsed.resources().memory_max_bytes(), Some(1_342_177_280));
        assert_eq!(parsed.resources().pids_max(), Some(1024));
        assert_eq!(parsed.to_keyfile(), VALID);
        assert_eq!(
            PermissionPolicy::parse(&parsed.to_keyfile()).unwrap(),
            parsed
        );
    }

    #[test]
    fn constructor_and_parser_produce_the_same_policy() {
        let constructed = PermissionPolicy::new()
            .with_network()
            .unwrap()
            .with_socket(PermissionSocket::PulseAudio)
            .unwrap()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_development()
            .unwrap()
            .with_filesystem("xdg-pictures", FilesystemAccess::Deny, false)
            .unwrap()
            .with_filesystem("xdg-download", FilesystemAccess::ReadWrite, true)
            .unwrap()
            .with_filesystem("~/Projects", FilesystemAccess::ReadWrite, true)
            .unwrap()
            .with_filesystem("/mnt/archive", FilesystemAccess::ReadOnly, false)
            .unwrap()
            .with_session_bus("org.mozilla.firefox", BusAccess::Own)
            .unwrap()
            .with_session_bus("org.freedesktop.FileManager1", BusAccess::Talk)
            .unwrap()
            .with_memory_high(1_073_741_824)
            .unwrap()
            .with_memory_max(1_342_177_280)
            .unwrap()
            .with_pids_max(1024)
            .unwrap();
        assert_eq!(constructed.to_keyfile(), VALID);
        assert_eq!(PermissionPolicy::parse(VALID).unwrap(), constructed);
    }

    #[test]
    fn empty_policy_has_a_versioned_canonical_form() {
        let parsed = PermissionPolicy::parse("# no grants\nformat = 1\n").unwrap();
        assert_eq!(parsed, PermissionPolicy::new());
        assert_eq!(parsed.to_keyfile(), "format=1\n");
    }

    #[test]
    fn jail_subset_is_wayland_network_filesystems_resources_and_owned_names() {
        let admitted = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("xdg-download", FilesystemAccess::ReadWrite, false)
            .unwrap();
        assert_eq!(admitted.unhonoured_request(), None);
        assert_eq!(
            PermissionPolicy::new().unhonoured_request().as_deref(),
            Some("a launch with no wayland socket")
        );
        assert_eq!(
            admitted
                .clone()
                .with_socket(PermissionSocket::PulseAudio)
                .unwrap()
                .unhonoured_request()
                .as_deref(),
            Some("the pulseaudio socket")
        );
        assert_eq!(
            admitted
                .clone()
                .with_network()
                .unwrap()
                .unhonoured_request(),
            None
        );
        assert_eq!(
            admitted
                .clone()
                .with_development()
                .unwrap()
                .unhonoured_request()
                .as_deref(),
            Some("features=allow-devel")
        );
        let resource_policy = admitted
            .clone()
            .with_memory_high(4096)
            .unwrap()
            .with_memory_max(8192)
            .unwrap()
            .with_pids_max(8)
            .unwrap();
        assert_eq!(resource_policy.unhonoured_request(), None);
        assert_eq!(
            admitted.clone().with_pids_max(8).unwrap().unhonoured_request(),
            None
        );

        // An `own` entry is honoured; `see` and `talk` are not, and the
        // refusal names the access and the name so an operator can find the
        // line.
        assert_eq!(
            admitted
                .clone()
                .with_session_bus("org.mozilla.firefox", BusAccess::Own)
                .unwrap()
                .unhonoured_request(),
            None
        );
        // The file's own ceiling is larger than what a launch can deliver, so
        // the grader has to charge the smaller one — or an operator learns
        // about it from the broker's wire reader at launch, in a message
        // about a list that "cannot be read".
        let mut many = admitted.clone();
        for index in 0..MAX_OWNED_BUS_NAMES {
            many = many
                .with_session_bus(&format!("org.example.N{index}"), BusAccess::Own)
                .unwrap();
        }
        assert_eq!(many.unhonoured_request(), None, "at the ceiling");
        let over = many
            .with_session_bus("org.example.Spill", BusAccess::Own)
            .unwrap();
        assert_eq!(
            over.unhonoured_request().as_deref(),
            Some(
                format!(
                    "{} `own' entries, more than the {MAX_OWNED_BUS_NAMES} a broker will record",
                    MAX_OWNED_BUS_NAMES + 1
                )
                .as_str()
            )
        );

        for (access, spelling) in [(BusAccess::See, "see"), (BusAccess::Talk, "talk")] {
            assert_eq!(
                admitted
                    .clone()
                    .with_session_bus("org.freedesktop.FileManager1", access)
                    .unwrap()
                    .unhonoured_request()
                    .as_deref(),
                Some(
                    format!("`{spelling}\' access to org.freedesktop.FileManager1").as_str()
                )
            );
        }
    }

    #[test]
    fn comments_layout_and_section_order_canonicalize_away() {
        let text = " # operator policy\n format = 1\n\n[Resources]\n pids-max = 32\n\n[Context]\n sockets = pulseaudio ; wayland\n";
        let parsed = PermissionPolicy::parse(text).unwrap();
        assert_eq!(
            parsed.to_keyfile(),
            "format=1\n\n[Context]\nsockets=wayland;pulseaudio\n\n[Resources]\npids-max=32\n"
        );
    }

    #[test]
    fn malformed_structure_unknown_intent_and_duplicates_are_refused() {
        for (text, reason) in [
            ("", "empty"),
            ("format=1", "trailing newline"),
            ("format=1\r\n", "carriage return"),
            ("format=1\0\n", "NUL"),
            ("# absent\n", "missing `format'"),
            ("format=3\n", "unsupported"),
            ("format=2\n", "requires cpu-max"),
            ("format=1\nformat=1\n", "duplicate"),
            ("unknown=1\n", "unknown permission root key"),
            ("format=1\n[Unknown]\n", "unknown permission section"),
            ("format=1\n[Context]\n[Context]\n", "duplicate [Context]"),
            (
                "[Context]\nformat=1\n",
                "unknown [Context] key \"format\"",
            ),
            ("format=1\n[Context]\nnot a key\n", "expected key=value"),
            ("format=1\n[Context]\n=network\n", "empty key"),
            ("format=1\n[Context]\nshared=\n", "empty value"),
            ("format=1\n[Context]\nunknown=value\n", "unknown [Context]"),
            (
                "format=1\n[Filesystem]\nxdg-download=rw\nxdg-download=ro\n",
                "duplicate filesystem",
            ),
            (
                "format=1\n[Session Bus Policy]\norg.example.Service=see\norg.example.Service=talk\n",
                "duplicate session bus",
            ),
            (
                "format=1\n[Resources]\npids-max=4\npids-max=5\n",
                "duplicate resource",
            ),
        ] {
            let got = error(text);
            assert!(got.contains(reason), "{text:?}: {got}");
        }
    }

    #[test]
    fn context_is_a_closed_typed_vocabulary() {
        for (line, reason) in [
            ("shared=ipc", "only `network'"),
            ("shared=network;network", "duplicate"),
            ("shared=network;", "empty list"),
            ("sockets=x11", "unknown application socket"),
            ("sockets=wayland;wayland", "duplicate value"),
            ("features=devel", "only `allow-devel'"),
            ("devices=dri", "recognized but unavailable"),
            ("devices=dri;unknown", "unknown application device"),
            ("devices=tty", "fresh-terminal acquisition"),
            ("devices=all", "unknown application device"),
        ] {
            let text = format!("format=1\n[Context]\n{line}\n");
            let got = error(&text);
            assert!(got.contains(reason), "{line:?}: {got}");
        }
        for key in ["shared", "sockets", "features"] {
            let value = match key {
                "shared" => "network",
                "sockets" => "wayland",
                "features" => "allow-devel",
                _ => "",
            };
            let text = format!("format=1\n[Context]\n{key}={value}\n{key}={value}\n");
            let got = error(&text);
            assert!(got.contains("duplicate [Context]"), "{key}: {got}");
        }
    }

    #[test]
    fn filesystem_locations_and_access_are_typed_and_bounded() {
        for location in [
            "xdg-download",
            "xdg-documents",
            "xdg-pictures",
            "xdg-music",
            "xdg-videos",
            "xdg-desktop",
            "~/relative/path",
            "~/.local-other",
            "/mnt/media",
            "/td/other",
            "/var/log",
        ] {
            PermissionPolicy::new()
                .with_filesystem(location, FilesystemAccess::ReadOnly, false)
                .unwrap();
        }
        for (location, create, reason) in [
            ("host", false, "blanket"),
            ("home", false, "blanket"),
            ("~", false, "blanket"),
            ("relative", false, "must be an xdg name"),
            ("/mnt/a=b", false, "may not contain `='"),
            ("~/../escape", false, "\"..\" component"),
            ("~/a//b", false, "empty component"),
            ("~/.local", false, "Flatpak repository"),
            ("~/.local/share", false, "Flatpak repository"),
            ("~/.local/share/flatpak", false, "Flatpak repository"),
            ("/", false, "root `/`"),
            ("/", true, "root `/`"),
            ("/var/lib", false, "reserved filesystem tree"),
            ("/mnt/new", true, "only for xdg"),
        ] {
            let got = PermissionPolicy::new()
                .with_filesystem(location, FilesystemAccess::ReadOnly, create)
                .unwrap_err();
            assert!(got.contains(reason), "{location:?}: {got}");
        }
        for reserved in RESERVED_FILESYSTEM_TREES {
            let child = format!("{reserved}/child");
            for location in [*reserved, child.as_str()] {
                let got = PermissionPolicy::new()
                    .with_filesystem(location, FilesystemAccess::ReadOnly, false)
                    .unwrap_err();
                assert!(
                    got.contains("reserved filesystem tree"),
                    "{location:?}: {got}"
                );
            }
        }
        for mode in ["", "read-only", "create", "deny:create", "rw:unknown"] {
            let text = format!("format=1\n[Filesystem]\nxdg-download={mode}\n");
            let got = error(&text);
            assert!(
                got.contains("empty value") || got.contains("filesystem permission must"),
                "{mode:?}: {got}"
            );
        }
        assert!(PermissionPolicy::new()
            .with_filesystem("xdg-download", FilesystemAccess::Deny, true)
            .unwrap_err()
            .contains("denied"));

        let non_breaking_space = "~/path\u{a0}";
        let text = format!("format=1\n[Filesystem]\n{non_breaking_space}=ro\n");
        assert!(error(&text).contains("may not begin or end with whitespace"));
        assert!(PermissionPolicy::new()
            .with_filesystem(non_breaking_space, FilesystemAccess::ReadOnly, false)
            .unwrap_err()
            .contains("may not begin or end with whitespace"));

        let mut policy = PermissionPolicy::new();
        for index in 0..MAX_FILESYSTEM_ENTRIES {
            policy = policy
                .with_filesystem(
                    &format!("~/grant-{index}"),
                    FilesystemAccess::ReadOnly,
                    false,
                )
                .unwrap();
        }
        assert!(policy
            .with_filesystem("~/too-many", FilesystemAccess::ReadOnly, false)
            .unwrap_err()
            .contains("at most 128"));
    }

    #[test]
    fn session_bus_names_and_access_are_exact() {
        assert!(BusAccess::Own.allows(BusAccess::Own));
        assert!(BusAccess::Own.allows(BusAccess::Talk));
        assert!(BusAccess::Own.allows(BusAccess::See));
        assert!(BusAccess::Talk.allows(BusAccess::Talk));
        assert!(BusAccess::Talk.allows(BusAccess::See));
        assert!(!BusAccess::Talk.allows(BusAccess::Own));
        assert!(BusAccess::See.allows(BusAccess::See));
        assert!(!BusAccess::See.allows(BusAccess::Talk));
        assert!(!BusAccess::See.allows(BusAccess::Own));

        for name in [
            "org.example.Service",
            "org.example.service-1",
            "_org.example",
        ] {
            PermissionPolicy::new()
                .with_session_bus(name, BusAccess::See)
                .unwrap();
        }
        for (name, reason) in [
            ("service", "at least one"),
            ("org..Service", "empty component"),
            ("org.7zip.Service", "must begin"),
            (":1.20", "must begin"),
            ("org.example.*", "must begin"),
            ("org.example.Service/Child", "outside"),
        ] {
            let got = PermissionPolicy::new()
                .with_session_bus(name, BusAccess::Talk)
                .unwrap_err();
            assert!(got.contains(reason), "{name:?}: {got}");
        }
        for access in ["", "call", "See", "own-all"] {
            let text = format!("format=1\n[Session Bus Policy]\norg.example.Service={access}\n");
            let got = error(&text);
            assert!(
                got.contains("empty value") || got.contains("must be `see'"),
                "{access:?}: {got}"
            );
        }
    }

    #[test]
    fn resource_limits_match_the_cgroup_contract() {
        let cpu_policy = "format=2\n\n[Resources]\nmemory-high=50331648\nmemory-max=67108864\npids-max=32\ncpu-max=50000 100000\n";
        assert_eq!(
            PermissionPolicy::parse(cpu_policy).unwrap().to_keyfile(),
            cpu_policy
        );

        for (line, reason) in [
            ("memory-high=0", "greater than zero"),
            ("memory-max=01", "canonical positive"),
            ("memory-max=max", "unsigned decimal"),
            (
                "memory-max=9223372036854771712",
                "maximum admitted byte count 9223372036854767616",
            ),
            (
                "memory-max=99999999999999999999999",
                "maximum admitted byte count 9223372036854767616",
            ),
            ("pids-max=0", "greater than zero"),
            ("pids-max=4194305", "kernel task limit 4194304"),
            ("pids-max=4294967296", "kernel task limit 4194304"),
            (
                "pids-max=99999999999999999999999",
                "kernel task limit 4194304",
            ),
            (
                "cpu-max=50000 100000",
                "requires application permission format=2",
            ),
            ("unknown=1", "unknown [Resources]"),
        ] {
            let text = format!("format=1\n[Resources]\n{line}\n");
            let got = error(&text);
            assert!(got.contains(reason), "{line:?}: {got}");
        }
        let got = error("format=1\n[Resources]\nmemory-high=1024\nmemory-max=1024\n");
        assert!(
            got.starts_with("application permission file line 4:"),
            "{got}"
        );
        assert!(got.contains("memory-high must be below"), "{got}");

        PermissionPolicy::new()
            .with_memory_max(MAX_MEMORY_BYTES)
            .unwrap();
        assert!(PermissionPolicy::new()
            .with_memory_max(MAX_MEMORY_BYTES + 1)
            .unwrap_err()
            .contains("maximum admitted byte count 9223372036854767616"));

        PermissionPolicy::new().with_pids_max(MAX_PIDS).unwrap();
        assert!(PermissionPolicy::new()
            .with_pids_max(MAX_PIDS + 1)
            .unwrap_err()
            .contains("kernel task limit 4194304"));

        let defaults = PermissionPolicy::new()
            .resources()
            .complete_or_default()
            .unwrap();
        assert_eq!(
            defaults.memory_high_bytes(),
            Some(DEFAULT_MEMORY_HIGH_BYTES)
        );
        assert_eq!(defaults.memory_max_bytes(), Some(DEFAULT_MEMORY_MAX_BYTES));
        assert_eq!(defaults.pids_max(), Some(DEFAULT_PIDS_MAX));
        let default_cpu = defaults.cpu_max().unwrap();
        assert_eq!(default_cpu.quota_usec(), DEFAULT_CPU_QUOTA_USEC);
        assert_eq!(default_cpu.period_usec(), DEFAULT_CPU_PERIOD_USEC);

        let typed_cpu = PermissionPolicy::new()
            .with_memory_high(48 * 1024 * 1024)
            .unwrap()
            .with_memory_max(64 * 1024 * 1024)
            .unwrap()
            .with_pids_max(32)
            .unwrap()
            .with_cpu_max(50_000, 100_000)
            .unwrap();
        assert_eq!(typed_cpu.to_keyfile(), cpu_policy);
        for (value, reason) in [
            ("999 100000", "at least 1000"),
            ("17592186044416 100000", "pinned kernel limit"),
            ("50000 999", "at least 1000"),
            ("50000 1000001", "pinned kernel limit"),
            ("50000", "quota and period"),
            ("50000  100000", "exactly one ASCII space"),
            ("50000 100000 extra", "exactly one ASCII space"),
            ("50000\t100000", "quota and period"),
            ("50000\u{000c}100000", "quota and period"),
            ("max 100000", "unsigned decimal"),
        ] {
            let text = format!(
                "format=2\n[Resources]\nmemory-high=50331648\nmemory-max=67108864\npids-max=32\ncpu-max={value}\n"
            );
            let got = error(&text);
            assert!(got.contains(reason), "{value:?}: {got}");
        }

        let incomplete = PermissionPolicy::new()
            .with_pids_max(32)
            .unwrap()
            .resources()
            .complete_or_default()
            .unwrap_err();
        assert!(incomplete.contains("must set memory-high, memory-max and pids-max"));

        let unaligned = PermissionPolicy::new()
            .with_memory_high(DEFAULT_MEMORY_HIGH_BYTES + 1)
            .unwrap()
            .with_memory_max(DEFAULT_MEMORY_MAX_BYTES)
            .unwrap()
            .with_pids_max(DEFAULT_PIDS_MAX)
            .unwrap()
            .resources()
            .complete_or_default()
            .unwrap_err();
        assert!(unaligned.contains("4096-byte target page size"));
    }

    #[test]
    fn applications_cannot_own_broker_or_portal_names() {
        for name in [
            "org.freedesktop.DBus",
            "org.freedesktop.portal",
            "org.freedesktop.portal.Desktop",
            "org.freedesktop.impl.portal",
            "org.freedesktop.impl.portal.Access",
        ] {
            let got = PermissionPolicy::new()
                .with_session_bus(name, BusAccess::Own)
                .unwrap_err();
            assert!(got.contains("reserved session bus name"), "{name}: {got}");
            PermissionPolicy::new()
                .with_session_bus(name, BusAccess::Talk)
                .unwrap();
        }
    }

    #[test]
    fn aggregate_and_entry_counts_are_bounded() {
        let oversized = format!("#{}\nformat=1\n", "x".repeat(MAX_PERMISSION_FILE_BYTES));
        assert!(error(&oversized).starts_with("application permission file is "));

        let mut compact = String::from("format=1\n[Filesystem]\n");
        for (index, length) in [4086usize, 4087, 4086, 4087].into_iter().enumerate() {
            let prefix = format!("/mnt/{index}-");
            let location = format!("{prefix}{}", "x".repeat(length - prefix.len()));
            compact.push_str(&location);
            compact.push_str("=ro\n");
        }
        assert_eq!(compact.len(), MAX_PERMISSION_FILE_BYTES);
        assert!(error(&compact).contains("would be 16385 bytes"));

        let long_name = format!("org.{}.Service", "a".repeat(MAX_BUS_NAME_BYTES));
        assert!(PermissionPolicy::new()
            .with_session_bus(&long_name, BusAccess::See)
            .unwrap_err()
            .contains("limit is 255"));

        let mut policy = PermissionPolicy::new();
        for index in 0..MAX_BUS_POLICY_ENTRIES {
            policy = policy
                .with_session_bus(&format!("org.example.Service{index}"), BusAccess::See)
                .unwrap();
        }
        assert!(policy
            .with_session_bus("org.example.TooMany", BusAccess::See)
            .unwrap_err()
            .contains("at most 128"));

        let mut authored_filesystems = String::from("format=1\n[Filesystem]\n");
        for index in 0..=MAX_FILESYSTEM_ENTRIES {
            authored_filesystems.push_str(&format!("~/grant-{index}=ro\n"));
        }
        assert!(error(&authored_filesystems).contains("at most 128 filesystem entries"));

        let mut authored_bus = String::from("format=1\n[Session Bus Policy]\n");
        for index in 0..=MAX_BUS_POLICY_ENTRIES {
            authored_bus.push_str(&format!("org.example.Service{index}=see\n"));
        }
        assert!(error(&authored_bus).contains("at most 128 session bus entries"));

        let mut aggregate = PermissionPolicy::new();
        let long_component = "x".repeat(200);
        let mut stopped = false;
        for index in 0..MAX_FILESYSTEM_ENTRIES {
            let location = format!("~/grant-{index}-{long_component}");
            match aggregate
                .clone()
                .with_filesystem(&location, FilesystemAccess::ReadOnly, false)
            {
                Ok(next) => aggregate = next,
                Err(got) => {
                    assert!(got.contains("limit is 16384"), "{got}");
                    stopped = true;
                    break;
                }
            }
        }
        assert!(stopped, "aggregate rendering must reach the file bound");
    }

    #[test]
    fn parser_errors_name_the_authored_line() {
        let got = error("format=1\n\n[Filesystem]\n/usr/share=ro\n");
        assert!(
            got.starts_with("application permission file line 4:"),
            "{got}"
        );
        assert!(got.contains("reserved filesystem tree"), "{got}");
    }
}

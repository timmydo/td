//! Rootless memory scheduling shared by one user's hosted checks.
//!
//! The per-user host creates a fixed set of 1 GiB token files. Hosted check
//! requests, heavy gates, and persistent-daemon builds hold exclusive locks on
//! those same files while they run. Kernel release of `flock` on process death
//! makes grants crash-safe without a privileged coordinator or caller knobs.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GIB: u64 = 1024 * 1024 * 1024;
// One-GiB admission granularity lets a finite per-user/container envelope run
// one serialized low-memory gate without weakening the 2-GiB-per-compiler-job
// rule below. Larger hosts receive the same byte grants as before.
pub const TOKEN_BYTES: u64 = GIB;
pub const MAX_WORK_BYTES: u64 = 32 * GIB;
pub const BUILD_JOB_TOKEN_BYTES: u64 = 2 * GIB;

pub const HOST_CHILD_ENV: &str = "TD_CHECK_HOST_CHILD";
pub const HOST_RUNTIME_ENV: &str = "TD_CHECK_HOST_RUNTIME";
pub const TOKEN_DIR_ENV: &str = "TD_CHECK_HOST_TOKEN_DIR";
pub const TOKEN_COUNT_ENV: &str = "TD_CHECK_HOST_TOKEN_COUNT";
pub const BASE_TOKENS_ENV: &str = "TD_CHECK_HOST_BASE_TOKENS";
pub const GATE_TOKENS_ENV: &str = "TD_CHECK_HOST_GATE_TOKENS";
pub const RESERVE_BYTES_ENV: &str = "TD_CHECK_HOST_RESERVE_BYTES";
pub const JOB_BUDGET_ENV: &str = "TD_CHECK_JOB_BUDGET_BYTES";
pub const GATE_GRANT_HELD_ENV: &str = "TD_CHECK_HOST_GATE_GRANT_HELD";
pub const GATE_REQUEST_LOCK_ENV: &str = "TD_CHECK_HOST_GATE_REQUEST_LOCK";

const MIN_RESERVE_BYTES: u64 = 2 * GIB;
const MAX_RESERVE_BYTES: u64 = 8 * GIB;
const MAX_TOKEN_COUNT: usize = 64;
const MEMORY_PSI_LIMIT: f64 = 10.0;
const ADMISSION_LOCK: &str = "admission.lock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBudget {
    pub work_bytes: u64,
    pub reserve_bytes: u64,
    pub token_count: usize,
    pub base_tokens: usize,
    pub gate_tokens: usize,
}

impl HostBudget {
    pub fn discover() -> Result<Self, String> {
        // Hosted commands are children of this coordinator, so its finite
        // ancestors are the execution envelope that matters. A submitting
        // shell's separate cgroup is deliberately not an authorization
        // boundary in this cooperative rootless design.
        let physical = meminfo_value_bytes("MemTotal:")
            .ok_or_else(|| "cannot read MemTotal from /proc/meminfo".to_string())?;
        let mut finite_headrooms = Vec::new();
        if let Some(nodes) = current_cgroup_ancestors() {
            for node in nodes {
                if let Some(limit) = read_limit(&node.join("memory.max"))? {
                    let current = read_number(&node.join("memory.current")).ok_or_else(|| {
                        format!("cannot read {}", node.join("memory.current").display())
                    })?;
                    finite_headrooms.push(limit.saturating_sub(current));
                }
            }
        }
        for node in v1_memory_cgroup_ancestors()? {
            let limit_path = node.join("memory.limit_in_bytes");
            let current_path = node.join("memory.usage_in_bytes");
            let limit = read_required_number(&limit_path)?;
            let current = read_required_number(&current_path)?;
            finite_headrooms.push(limit.saturating_sub(current));
        }
        Self::from_observed(physical, &finite_headrooms)
    }

    fn from_observed(physical: u64, finite_headrooms: &[u64]) -> Result<Self, String> {
        let proportional = physical / 4;
        let reserve_bytes = proportional.clamp(MIN_RESERVE_BYTES, MAX_RESERVE_BYTES);
        let mut work_bytes = physical.saturating_sub(reserve_bytes).min(MAX_WORK_BYTES);
        // The physical-MemAvailable arm already preserves the full host
        // reserve. A nested finite envelope needs its own fixed emergency
        // margin, not a second proportional subtraction from the same work.
        let ancestor_reserve = MIN_RESERVE_BYTES;
        for headroom in finite_headrooms {
            work_bytes = work_bytes.min(headroom.saturating_sub(ancestor_reserve));
        }
        let token_count = usize::try_from(work_bytes / TOKEN_BYTES)
            .unwrap_or(MAX_TOKEN_COUNT)
            .min(MAX_TOKEN_COUNT);
        if token_count < 3 {
            return Err(format!(
                "only {} MiB is available after the host reserve; hosted checks need at least three 1 GiB memory tokens",
                work_bytes / (1024 * 1024)
            ));
        }
        // Preserve the established byte grants above 6/12 GiB while allowing
        // a 3-GiB envelope to serialize one 2-GiB gate behind a 1-GiB host
        // control-plane grant.
        let base_tokens = if token_count >= 12 {
            4
        } else if token_count >= 6 {
            2
        } else {
            1
        };
        let gate_tokens = 8.min(token_count.saturating_sub(base_tokens)).max(2);
        Ok(Self {
            work_bytes: u64::try_from(token_count)
                .unwrap_or(u64::MAX)
                .saturating_mul(TOKEN_BYTES),
            reserve_bytes,
            token_count,
            base_tokens,
            gate_tokens,
        })
    }
}

#[derive(Debug)]
pub struct MemoryPermit {
    _files: Vec<File>,
    bytes: u64,
}

impl MemoryPermit {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Clone)]
pub struct TokenPool {
    dir: PathBuf,
    count: usize,
    reserve_bytes: u64,
}

impl TokenPool {
    pub fn create(dir: &Path, budget: &HostBudget) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create memory-token directory {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("protect memory-token directory {}: {e}", dir.display()))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(ADMISSION_LOCK))
            .map_err(|e| format!("create memory-token admission lock: {e}"))?;
        for i in 0..budget.token_count {
            let path = dir.join(format!("token-{i}"));
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("create memory token {}: {e}", path.display()))?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            count: budget.token_count,
            reserve_bytes: budget.reserve_bytes,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        if std::env::var_os(HOST_CHILD_ENV).is_none() {
            return Err("the check is not running through its per-user host".to_string());
        }
        let dir = std::env::var_os(TOKEN_DIR_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| format!("hosted check is missing {TOKEN_DIR_ENV}"))?;
        let count = parse_env_usize(TOKEN_COUNT_ENV)?;
        if count == 0 || count > MAX_TOKEN_COUNT {
            return Err(format!(
                "hosted check has invalid {TOKEN_COUNT_ENV}={count}"
            ));
        }
        let reserve_bytes = parse_env_u64(RESERVE_BYTES_ENV)?;
        if !dir.is_dir() {
            return Err(format!(
                "host memory-token directory {} is unavailable",
                dir.display()
            ));
        }
        Ok(Self {
            dir,
            count,
            reserve_bytes,
        })
    }

    pub fn acquire(
        &self,
        tokens: usize,
        leave_free: usize,
        aborted: &dyn Fn() -> bool,
    ) -> Result<MemoryPermit, String> {
        if tokens == 0 || tokens.saturating_add(leave_free) > self.count {
            return Err(format!(
                "memory grant asks for {tokens} token(s) plus {leave_free} reserved from a {}-token pool",
                self.count
            ));
        }
        let paths: Vec<PathBuf> = (0..self.count)
            .map(|i| self.dir.join(format!("token-{i}")))
            .collect();
        loop {
            // Selecting several token files and proving `leave_free` is one
            // admission transaction. Without this short lock, concurrent base
            // requests can each observe the same not-yet-claimed free files and
            // collectively consume the gate capacity all of them promised to
            // preserve.
            let admission_path = self.dir.join(ADMISSION_LOCK);
            let admission = OpenOptions::new()
                .append(true)
                .open(&admission_path)
                .map_err(|e| format!("open memory-token admission lock: {e}"))?;
            match crate::sys::flock_try_exclusive(admission.as_raw_fd()) {
                Ok(true) => {}
                Ok(false) => {
                    drop(admission);
                    if aborted() {
                        return Err("memory grant cancelled before admission".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(e) => return Err(format!("lock {}: {e}", admission_path.display())),
            }
            let mut held = Vec::with_capacity(tokens);
            let mut locked_elsewhere = 0usize;
            for path in &paths {
                let file = OpenOptions::new()
                    .append(true)
                    .open(path)
                    .map_err(|e| format!("open memory token {}: {e}", path.display()))?;
                match crate::sys::flock_try_exclusive(file.as_raw_fd()) {
                    Ok(true) if held.len() < tokens => held.push(file),
                    Ok(true) => {}
                    Ok(false) => locked_elsewhere = locked_elsewhere.saturating_add(1),
                    Err(e) => return Err(format!("lock memory token {}: {e}", path.display())),
                }
            }
            let remaining = self.count.saturating_sub(locked_elsewhere + held.len());
            let requested_bytes = u64::try_from(tokens)
                .unwrap_or(u64::MAX)
                .saturating_mul(TOKEN_BYTES);
            let memory_ok = memory_admission_bytes(self.reserve_bytes)
                .map(|bytes| bytes >= requested_bytes)
                .unwrap_or(false);
            let pressure_ok = memory_psi_some_avg10()
                .map(|pressure| pressure < MEMORY_PSI_LIMIT)
                .unwrap_or(true);
            if held.len() == tokens && remaining >= leave_free && memory_ok && pressure_ok {
                drop(admission);
                return Ok(MemoryPermit {
                    _files: held,
                    bytes: requested_bytes,
                });
            }
            drop(held);
            drop(admission);
            if aborted() {
                return Err("memory grant cancelled before admission".to_string());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

pub fn base_permit(
    pool: &TokenPool,
    budget: &HostBudget,
    aborted: &dyn Fn() -> bool,
) -> Result<MemoryPermit, String> {
    pool.acquire(budget.base_tokens, budget.gate_tokens, aborted)
}

pub fn gate_permit(aborted: &dyn Fn() -> bool) -> Result<MemoryPermit, String> {
    let pool = TokenPool::from_env()?;
    pool.acquire(parse_env_usize(GATE_TOKENS_ENV)?, 0, aborted)
}

pub fn build_permit(aborted: &dyn Fn() -> bool) -> Result<MemoryPermit, String> {
    gate_permit(aborted)
}

pub fn create_gate_request_lock() -> Result<PathBuf, String> {
    let pool = TokenPool::from_env()?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = pool
        .dir
        .join(format!("gate-request-{}-{serial}.lock", std::process::id()));
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("create gate daemon-request lock {}: {e}", path.display()))?;
    Ok(path)
}

pub fn lock_gate_request() -> Result<File, String> {
    let pool = TokenPool::from_env()?;
    let path = std::env::var_os(GATE_REQUEST_LOCK_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("granted gate is missing {GATE_REQUEST_LOCK_ENV}"))?;
    if path.parent() != Some(pool.dir.as_path()) {
        return Err(format!(
            "gate daemon-request lock {} is outside the hosted token directory",
            path.display()
        ));
    }
    let file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("open gate daemon-request lock {}: {e}", path.display()))?;
    crate::sys::flock_exclusive(file.as_raw_fd())
        .map_err(|e| format!("lock gate daemon requests {}: {e}", path.display()))?;
    Ok(file)
}

pub fn hosted_gate_capacity() -> usize {
    if std::env::var_os(HOST_CHILD_ENV).is_none() {
        return 1;
    }
    match (
        parse_env_usize(TOKEN_COUNT_ENV),
        parse_env_usize(GATE_TOKENS_ENV),
    ) {
        (Ok(total), Ok(per_gate)) if per_gate > 0 => total / per_gate,
        _ => 1,
    }
    .max(1)
}

pub fn request_job_budget() -> Option<u64> {
    std::env::var(JOB_BUDGET_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
}

pub fn jobs_for_budget(bytes: u64, cpu: usize) -> usize {
    usize::try_from(bytes / BUILD_JOB_TOKEN_BYTES)
        .unwrap_or(usize::MAX)
        .max(1)
        .min(cpu.max(1))
}

pub fn build_jobs() -> usize {
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    request_job_budget()
        .map(|bytes| jobs_for_budget(bytes, cpu))
        .unwrap_or(1)
}

/// The live cancellation backstop keeps a proportional emergency margin from
/// host-wide MemAvailable and a fixed 2-GiB margin inside each nested finite
/// envelope. Taking one minimum and comparing it with the host margin would
/// spend the proportional reserve twice inside a per-user/container limit.
pub fn emergency_memory_available(global_emergency: u64) -> Option<bool> {
    if meminfo_value_bytes("MemAvailable:")? < global_emergency {
        return Some(false);
    }
    if let Some(nodes) = current_cgroup_ancestors() {
        for node in nodes {
            let max = read_limit(&node.join("memory.max")).ok()?;
            if let Some(max) = max {
                let current = read_number(&node.join("memory.current"))?;
                if max.saturating_sub(current) < MIN_RESERVE_BYTES {
                    return Some(false);
                }
            }
        }
    }
    for node in v1_memory_cgroup_ancestors().ok()? {
        let limit = read_required_number(&node.join("memory.limit_in_bytes")).ok()?;
        let current = read_required_number(&node.join("memory.usage_in_bytes")).ok()?;
        if limit.saturating_sub(current) < MIN_RESERVE_BYTES {
            return Some(false);
        }
    }
    Some(true)
}

fn memory_admission_bytes(global_reserve: u64) -> Option<u64> {
    let mut available = meminfo_value_bytes("MemAvailable:")?.saturating_sub(global_reserve);
    if let Some(nodes) = current_cgroup_ancestors() {
        for node in nodes {
            let limit = read_limit(&node.join("memory.max")).ok()?;
            if let Some(limit) = limit {
                let current = read_number(&node.join("memory.current"))?;
                available = available.min(
                    limit
                        .saturating_sub(current)
                        .saturating_sub(MIN_RESERVE_BYTES),
                );
            }
        }
    }
    for node in v1_memory_cgroup_ancestors().ok()? {
        let limit = read_required_number(&node.join("memory.limit_in_bytes")).ok()?;
        let current = read_required_number(&node.join("memory.usage_in_bytes")).ok()?;
        available = available.min(
            limit
                .saturating_sub(current)
                .saturating_sub(MIN_RESERVE_BYTES),
        );
    }
    Some(available)
}

fn meminfo_value_bytes(prefix: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    value.checked_mul(1024)
}

fn current_cgroup_ancestors() -> Option<Vec<PathBuf>> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let mut path = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    let root = Path::new("/sys/fs/cgroup");
    let mut out = Vec::new();
    loop {
        out.push(path.clone());
        if path == root {
            break;
        }
        path = path.parent()?.to_path_buf();
        if !path.starts_with(root) {
            return None;
        }
    }
    Some(out)
}

fn decode_mount_field(value: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'\\') {
            let a = bytes.get(index.saturating_add(1)).copied();
            let b = bytes.get(index.saturating_add(2)).copied();
            let c = bytes.get(index.saturating_add(3)).copied();
            if let (Some(a), Some(b), Some(c)) = (a, b, c) {
                if (b'0'..=b'7').contains(&a)
                    && (b'0'..=b'7').contains(&b)
                    && (b'0'..=b'7').contains(&c)
                {
                    decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                    index = index.saturating_add(4);
                    continue;
                }
            }
        }
        if let Some(byte) = bytes.get(index) {
            decoded.push(*byte);
        }
        index = index.saturating_add(1);
    }
    PathBuf::from(std::ffi::OsString::from_vec(decoded))
}

fn v1_memory_membership(cgroups: &str) -> Result<Option<PathBuf>, String> {
    let mut membership: Option<PathBuf> = None;
    for line in cgroups.lines() {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next();
        let controllers = fields.next();
        let path = fields.next();
        let (Some(hierarchy), Some(controllers), Some(path)) = (hierarchy, controllers, path)
        else {
            continue;
        };
        if hierarchy == "0"
            || !controllers
                .split(',')
                .any(|controller| controller == "memory")
        {
            continue;
        }
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("v1 memory cgroup membership is not absolute".to_string());
        }
        match &membership {
            Some(existing) if existing != &path => {
                return Err("v1 memory controller has conflicting memberships".to_string())
            }
            Some(_) => {}
            None => membership = Some(path),
        }
    }
    Ok(membership)
}

fn v1_memory_mount(mountinfo: &str) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let mut mounts = Vec::new();
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut left_fields = left.split_whitespace();
        let root = left_fields.nth(3);
        let mountpoint = left_fields.next();
        let mut right_fields = right.split_whitespace();
        let fs_type = right_fields.next();
        let _source = right_fields.next();
        let super_options = right_fields.next();
        let (Some(root), Some(mountpoint), Some("cgroup"), Some(super_options)) =
            (root, mountpoint, fs_type, super_options)
        else {
            continue;
        };
        if super_options.split(',').any(|option| option == "memory") {
            mounts.push((decode_mount_field(root), decode_mount_field(mountpoint)));
        }
    }
    match mounts.as_slice() {
        [] => Ok(None),
        [mount] => Ok(Some(mount.clone())),
        _ => Err("v1 memory controller has multiple mounted hierarchies".to_string()),
    }
}

fn v1_memory_cgroup_ancestors_from(cgroups: &str, mountinfo: &str) -> Result<Vec<PathBuf>, String> {
    let Some(membership) = v1_memory_membership(cgroups)? else {
        return Ok(Vec::new());
    };
    let Some((mount_root, mountpoint)) = v1_memory_mount(mountinfo)? else {
        return Err("active v1 memory controller has no visible cgroup mount".to_string());
    };
    // A cgroup namespace or subtree bind can hide finite ancestors. v1 does not
    // expose their live usage through the child, so treating the visible root as
    // the whole envelope could over-admit when a sibling consumes the parent.
    if mount_root != Path::new("/") {
        return Err(format!(
            "v1 memory cgroup mount {} hides ancestors above {}",
            mountpoint.display(),
            mount_root.display()
        ));
    }
    let relative = membership.strip_prefix(Path::new("/")).map_err(|_| {
        format!(
            "v1 memory membership {} is outside its hierarchy root",
            membership.display()
        )
    })?;
    let mut path = mountpoint.join(relative);
    if !path.starts_with(&mountpoint) {
        return Err("v1 memory cgroup path escapes its mounted hierarchy".to_string());
    }
    let mut out = Vec::new();
    loop {
        out.push(path.clone());
        if path == mountpoint {
            break;
        }
        path = path
            .parent()
            .ok_or_else(|| "v1 memory cgroup path has no mounted-hierarchy parent".to_string())?
            .to_path_buf();
        if !path.starts_with(&mountpoint) {
            return Err("v1 memory cgroup ancestor escapes its mounted hierarchy".to_string());
        }
    }
    Ok(out)
}

fn v1_memory_cgroup_ancestors() -> Result<Vec<PathBuf>, String> {
    let cgroups = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("read /proc/self/cgroup for v1 memory accounting: {e}"))?;
    if v1_memory_membership(&cgroups)?.is_none() {
        return Ok(Vec::new());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read /proc/self/mountinfo for v1 memory accounting: {e}"))?;
    v1_memory_cgroup_ancestors_from(&cgroups, &mountinfo)
}

fn read_limit(path: &Path) -> Result<Option<u64>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let value = raw.trim();
    if value == "max" {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .map_err(|_| format!("{} contains a malformed memory limit", path.display()))
    }
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_required_number(path: &Path) -> Result<u64, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?
        .trim()
        .parse()
        .map_err(|_| format!("{} contains a malformed memory value", path.display()))
}

fn memory_psi_some_avg10() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    let line = text.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

fn parse_env_usize(name: &str) -> Result<usize, String> {
    std::env::var(name)
        .map_err(|_| format!("hosted check is missing {name}"))?
        .parse::<usize>()
        .map_err(|_| format!("hosted check has invalid {name}"))
}

fn parse_env_u64(name: &str) -> Result<u64, String> {
    std::env::var(name)
        .map_err(|_| format!("hosted check is missing {name}"))?
        .parse::<u64>()
        .map_err(|_| format!("hosted check has invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_follow_memory_before_cpu() {
        assert_eq!(jobs_for_budget(1, 16), 1);
        assert_eq!(jobs_for_budget(4 * GIB, 16), 2);
        assert_eq!(jobs_for_budget(64 * GIB, 3), 3);
    }

    #[test]
    fn discovered_budget_keeps_a_gate_available_after_base_grants() {
        for total in 3..=MAX_TOKEN_COUNT {
            let base = if total >= 12 {
                4
            } else if total >= 6 {
                2
            } else {
                1
            };
            let gate = 8.min(total.saturating_sub(base)).max(2);
            assert!(base + gate <= total);
        }
    }

    #[test]
    fn finite_ancestor_headroom_scales_the_user_pool_without_spending_host_reserve_twice() {
        let budget = HostBudget::from_observed(64 * GIB, &[11 * GIB]).unwrap();
        assert_eq!(budget.reserve_bytes, 8 * GIB);
        assert_eq!(budget.work_bytes, 9 * GIB);
        assert_eq!(budget.token_count, 9);
        assert_eq!(budget.base_tokens, 2);
        assert_eq!(budget.gate_tokens, 7);
    }

    #[test]
    fn a_three_gib_envelope_runs_one_serialized_low_memory_gate() {
        let budget = HostBudget::from_observed(64 * GIB, &[5 * GIB]).unwrap();
        assert_eq!(budget.work_bytes, 3 * GIB);
        assert_eq!(budget.token_count, 3);
        assert_eq!(budget.base_tokens, 1);
        assert_eq!(budget.gate_tokens, 2);
        assert_eq!(jobs_for_budget(2 * TOKEN_BYTES, 16), 1);
    }

    #[test]
    fn v1_memory_controller_maps_every_visible_ancestor() {
        let cgroups =
            "7:cpu,cpuacct:/user.slice/session.scope\n5:memory:/user.slice/session.scope\n";
        let mountinfo = "31 24 0:27 / /sys/fs/cgroup/memory rw,nosuid,nodev,noexec,relatime - cgroup cgroup rw,memory\n";

        assert_eq!(
            v1_memory_cgroup_ancestors_from(cgroups, mountinfo).unwrap(),
            [
                PathBuf::from("/sys/fs/cgroup/memory/user.slice/session.scope"),
                PathBuf::from("/sys/fs/cgroup/memory/user.slice"),
                PathBuf::from("/sys/fs/cgroup/memory"),
            ]
        );
    }

    #[test]
    fn hybrid_hierarchy_still_accounts_its_v1_memory_controller() {
        let cgroups = "0::/user.slice/session.scope\n5:memory,blkio:/legacy/session.scope\n";
        let mountinfo = "31 24 0:27 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory,blkio\n32 24 0:28 / /sys/fs/cgroup/unified rw - cgroup2 cgroup rw\n";

        assert_eq!(
            v1_memory_cgroup_ancestors_from(cgroups, mountinfo).unwrap(),
            [
                PathBuf::from("/sys/fs/cgroup/memory/legacy/session.scope"),
                PathBuf::from("/sys/fs/cgroup/memory/legacy"),
                PathBuf::from("/sys/fs/cgroup/memory"),
            ]
        );
    }

    #[test]
    fn v1_memory_subtree_mount_fails_closed_when_ancestors_are_hidden() {
        let cgroups = "5:memory:/docker/container\n";
        let mountinfo =
            "31 24 0:27 /docker/container /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n";

        let error = v1_memory_cgroup_ancestors_from(cgroups, mountinfo).unwrap_err();
        assert!(error.contains("hides ancestors"), "{error}");
    }

    #[test]
    fn a_host_without_a_v1_memory_controller_keeps_the_v2_only_path() {
        let cgroups = "0::/user.slice/session.scope\n7:cpu,cpuacct:/user.slice/session.scope\n";

        assert!(v1_memory_cgroup_ancestors_from(cgroups, "")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn token_grants_share_one_pool_and_keep_the_reserved_width_free() {
        let dir =
            std::env::temp_dir().join(format!("td-check-memory-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let budget = HostBudget {
            work_bytes: 3 * TOKEN_BYTES,
            reserve_bytes: 0,
            token_count: 3,
            base_tokens: 1,
            gate_tokens: 2,
        };
        let pool = TokenPool::create(&dir, &budget).unwrap();
        let admission = OpenOptions::new()
            .append(true)
            .open(dir.join(ADMISSION_LOCK))
            .unwrap();
        crate::sys::flock_exclusive(admission.as_raw_fd()).unwrap();
        let serialized = pool.acquire(1, 2, &|| true).unwrap_err();
        assert!(serialized.contains("cancelled"));
        drop(admission);
        let base = pool.acquire(1, 2, &|| false).unwrap();
        assert_eq!(base.bytes(), TOKEN_BYTES);
        let gate = pool.acquire(2, 0, &|| false).unwrap();
        assert_eq!(gate.bytes(), 2 * TOKEN_BYTES);
        let blocked = pool.acquire(1, 0, &|| true).unwrap_err();
        assert!(blocked.contains("cancelled"));
        drop(gate);
        drop(base);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

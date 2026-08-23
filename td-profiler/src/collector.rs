use crate::cpuset;
use crate::event::{Event, Kind, StartIdentity};
use crate::perf;
use crate::raw;
use crate::report::{self, Meta};
use crate::symbol::Symbolizer;
use crate::sys;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_ROOT: &str = "/var/lib/td-profiler/captures";
pub const DEFAULT_INDEX: &str = "/etc/td-profiler-objects.tsv";
pub const DEFAULT_RATE_HZ: u32 = 99;
pub const DEFAULT_DURATION_SECS: u64 = 60;
pub const DEFAULT_RING_PAGES: usize = 64;
pub const MAX_CAPTURE_COUNT: usize = 24;
pub const MAX_CAPTURE_BYTES: u64 = raw::MAX_RAW_FILE_BYTES + 2 * report::MAX_REPORT_BYTES;
pub const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TERMINAL_RAW_RESERVE: u64 = 64 * 1024;
const NORMAL_RAW_BYTES: u64 = raw::MAX_RAW_FILE_BYTES - TERMINAL_RAW_RESERVE;
const NORMAL_RAW_EVENTS: usize = raw::MAX_DECODED_EVENTS - 2;
const MAX_TERMINAL_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_SNAPSHOT_TASKS: usize = 65_536;
const MAX_SNAPSHOT_MAPPINGS: usize = 262_144;
const MAX_PROC_MAPS_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROC_STAT_BYTES: u64 = 64 * 1024;
const STARTUP_ATTEMPTS: usize = 3;
const O_DIRECTORY: i32 = 0o200_000;
const O_NOFOLLOW: i32 = 0o400_000;
const O_CLOEXEC: i32 = 0o2_000_000;
const O_NONBLOCK: i32 = 0o4_000;
const COLLECTOR_LOCK: &str = "collector.lock";
const RETENTION_LOCK: &str = "retention.lock";

pub struct Config {
    pub root: PathBuf,
    pub index: Option<PathBuf>,
    pub deployment: String,
    pub profiler_build: String,
    pub uid: u32,
    pub gid: u32,
    pub rate_hz: u32,
    pub duration: Duration,
    pub once: bool,
}

pub fn run(config: Config) -> Result<(), String> {
    validate_config(&config)?;
    validate_capture_root(&config.root, config.uid, config.gid)?;
    let _collector_lock = CollectorLock::acquire(&config.root)?;
    recover_partials(&config.root)?;
    let boot_id = read_boot_id()?;
    let owner_start = self_start_ticks()?;

    let mut started = None;
    let mut last_error = String::new();
    for attempt in 1..=STARTUP_ATTEMPTS {
        match start_observation(&config) {
            Ok(observation) => {
                started = Some(observation);
                break;
            }
            Err(error) => {
                last_error = format!("startup attempt {attempt}/{STARTUP_ATTEMPTS}: {error}");
            }
        }
    }
    let mut observation = started.ok_or(last_error)?;
    verify_single_threaded()?;
    sys::drop_credentials(config.uid, config.gid)
        .map_err(|e| format!("drop profiler credentials: {e}"))?;
    verify_credentials(config.uid, config.gid)?;

    let mut sequence = 0u64;
    let mut reporter = None;
    prune(&config.root)?;
    loop {
        collect_capture(
            &config,
            &boot_id,
            owner_start,
            sequence,
            &mut observation,
            &mut reporter,
        )?;
        sequence = sequence.saturating_add(1);
        if config.once {
            finish_report(&mut observation, &mut reporter, true)?;
            return Ok(());
        }
    }
}

struct CollectorLock {
    _file: File,
}

pub(crate) struct RetentionLock {
    file: File,
    path: PathBuf,
}

impl RetentionLock {
    pub(crate) fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(RETENTION_LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o660)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC)
            .open(&path)
            .map_err(|e| format!("open retention lock {}: {e}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o660))
            .map_err(|e| format!("chmod retention lock {}: {e}", path.display()))?;
        file.lock()
            .map_err(|e| format!("lock retention root {}: {e}", root.display()))?;
        Ok(Self { file, path })
    }

    pub(crate) fn release(self) -> Result<(), String> {
        self.file
            .unlock()
            .map_err(|e| format!("unlock retention root {}: {e}", self.path.display()))
    }
}

impl CollectorLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(COLLECTOR_LOCK);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o640)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC)
            .open(&path)
            .map_err(|e| format!("open collector lock {}: {e}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o640))
            .map_err(|e| format!("chmod collector lock {}: {e}", path.display()))?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => {
                "another td-profiler collector owns the capture root".to_string()
            }
            fs::TryLockError::Error(error) => {
                format!("lock collector root {}: {error}", root.display())
            }
        })?;
        let owner = process_owner()?;
        file.set_len(0)
            .map_err(|e| format!("truncate collector lock {}: {e}", path.display()))?;
        file.write_all(owner.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("write collector lock {}: {e}", path.display()))?;
        sync_dir(root)?;
        Ok(Self { _file: file })
    }
}

struct Observation {
    cpus: Vec<sys::CpuEvents>,
    cpu_numbers: Vec<u32>,
    carry: Vec<Event>,
    symbolizer: Option<Symbolizer>,
    pending_sampling: Option<PendingSampling>,
}

struct PendingSampling {
    start_ns: u64,
    wall_start_seconds: u64,
    starts: Vec<Option<u64>>,
}

struct ReportWorker {
    partial: PathBuf,
    complete: PathBuf,
    root: PathBuf,
    failure_time_ns: u64,
    handle: thread::JoinHandle<WorkerOutput>,
}

struct WorkerOutput {
    symbolizer: Symbolizer,
    carry: Vec<Event>,
    result: Result<(), String>,
    reservation: Result<(), String>,
}

impl ReportWorker {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

fn start_observation(config: &Config) -> Result<Observation, String> {
    let paranoid = perf_event_paranoid()?;
    if paranoid < 1 {
        return Err(format!(
            "kernel.perf_event_paranoid is {paranoid}, expected at least 1 so only the privileged opener can request system-wide events"
        ));
    }
    let symbolizer = match config.index.as_deref() {
        Some(index) => match assert_store_mount() {
            Ok(()) => Symbolizer::from_index(Some(index)),
            Err(error) => Symbolizer::unavailable(error),
        },
        None => Symbolizer::default(),
    };
    let cpu_numbers = online_cpus()?;
    let mut cpus = Vec::with_capacity(cpu_numbers.len());
    for cpu in &cpu_numbers {
        let events = sys::CpuEvents::open(*cpu, config.rate_hz, DEFAULT_RING_PAGES).map_err(|e| {
            if matches!(e.raw_os_error(), Some(1 | 13)) {
                format!(
                    "unsupported-permission: open perf events for CPU {cpu} was denied ({e}); check kernel.perf_event_paranoid and service credentials"
                )
            } else {
                format!("open perf events for CPU {cpu}: {e}")
            }
        })?;
        events
            .enable_metadata()
            .map_err(|e| format!("enable CPU {cpu} metadata: {e}"))?;
        cpus.push(events);
    }
    let (mut carry, mut carry_bytes) = snapshot_tasks()?;
    // The read itself is the ordering fence: every following ring-head
    // snapshot is necessarily taken after this monotonic instant.
    sys::monotonic_ns().map_err(|e| format!("read startup end fence: {e}"))?;
    for cpu in &mut cpus {
        let number = cpu.cpu();
        let records = cpu
            .drain()
            .map_err(|e| format!("startup ring loss on CPU {number}: {e}"))?;
        for (sequence, record) in records {
            let event = perf::decode(&record, number, sequence)
                .map_err(|e| format!("startup record on CPU {number}: {e}"))?;
            match &event.kind {
                Kind::Lost { count, reason } => {
                    return Err(format!(
                        "startup ring reported {count} lost records on CPU {number}: {}",
                        String::from_utf8_lossy(reason)
                    ));
                }
                Kind::Error { message } => {
                    return Err(format!(
                        "startup ring reported a corrupt record on CPU {number}: {}",
                        String::from_utf8_lossy(message)
                    ));
                }
                _ => append_bounded(
                    &mut carry,
                    &mut carry_bytes,
                    event,
                    crate::state::MAX_CARRY_BYTES,
                    "startup baseline",
                )?,
            }
        }
    }
    // Reading the fence before the drain guarantees that every ring head
    // consumed above covers that fence. Records racing after the head snapshot
    // remain in the ring and are consumed before any later sample by time.
    let after = online_cpus()?;
    if after != cpu_numbers {
        return Err(format!(
            "online CPU topology changed during startup: before {cpu_numbers:?}, after {after:?}"
        ));
    }
    carry.sort_by_key(Event::ordering_key);
    Ok(Observation {
        cpus,
        cpu_numbers,
        carry,
        symbolizer: Some(symbolizer),
        pending_sampling: None,
    })
}

fn collect_capture(
    config: &Config,
    boot_id: &str,
    owner_start: u64,
    sequence: u64,
    observation: &mut Observation,
    reporter: &mut Option<ReportWorker>,
) -> Result<(), String> {
    let name = format!("{boot_id}.{}.{owner_start}.{sequence}", std::process::id());
    let partial = config.root.join(format!("{name}.partial"));
    let pending = observation.pending_sampling.take();
    let start_ns = pending
        .as_ref()
        .map(|pending| pending.start_ns)
        .map_or_else(
            || sys::monotonic_ns().map_err(|e| format!("read capture start: {e}")),
            Ok,
        )?;
    let wall_start_seconds = pending
        .as_ref()
        .map(|pending| pending.wall_start_seconds)
        .map_or_else(wall_seconds, Ok)?;
    let deadline =
        start_ns.saturating_add(config.duration.as_nanos().min(u128::from(u64::MAX)) as u64);
    let waiting_for_carry = reporter.is_some();
    let sampling_event_limit = if waiting_for_carry {
        NORMAL_RAW_EVENTS.saturating_sub(crate::state::MAX_CARRY_EVENTS)
    } else {
        NORMAL_RAW_EVENTS
    };
    let mut events = std::mem::take(&mut observation.carry);
    let mut raw_bytes = if waiting_for_carry {
        (raw::FILE_HEADER_BYTES as u64).saturating_add(crate::state::MAX_CARRY_BYTES)
    } else {
        raw::FILE_HEADER_BYTES as u64
    };
    for event in &events {
        let length = u64::try_from(raw::encoded_len(event).map_err(|e| e.to_string())?)
            .map_err(|_| "raw record length does not fit u64")?;
        raw_bytes = raw_bytes
            .checked_add(length)
            .ok_or("initial raw byte count overflow")?;
        if raw_bytes > NORMAL_RAW_BYTES {
            return Err("carry state exceeds the raw capture budget".into());
        }
    }
    let mut coverage_starts = pending
        .map(|pending| pending.starts)
        .unwrap_or_else(|| vec![None; observation.cpus.len()]);
    let mut coverage_ends = vec![None; observation.cpus.len()];
    let mut fatal_errors = Vec::new();
    let mut reservation_uncertain = false;
    let mut raw_full = false;
    let mut enabled: Vec<bool> = coverage_starts.iter().map(Option::is_some).collect();
    if !enabled.iter().any(|enabled| *enabled) {
        enable_sampling(
            &observation.cpus,
            &mut coverage_starts,
            &mut enabled,
            &mut fatal_errors,
        )?;
    }
    let mut disable_failed = vec![false; observation.cpus.len()];
    let mut loss_fences = vec![start_ns; observation.cpus.len()];

    while fatal_errors.is_empty() {
        if reporter.as_ref().is_some_and(ReportWorker::is_finished) {
            if let Err(error) = finish_report(observation, reporter, false) {
                fatal_errors.push(format!("finish prior capture: {error}"));
                reservation_uncertain = true;
                continue;
            }
        }
        let now = sys::monotonic_ns().map_err(|e| format!("read capture clock: {e}"))?;
        if now >= deadline && reporter.is_none() {
            break;
        }
        if raw_full {
            if reporter
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
            {
                for cpu in &mut observation.cpus {
                    let _ = cpu.drain();
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            break;
        }
        for (index, cpu) in observation.cpus.iter_mut().enumerate() {
            let loss_fence = loss_fences.get(index).copied().unwrap_or(start_ns);
            let outcome = drain_cpu(
                cpu,
                &mut events,
                &mut raw_bytes,
                loss_fence,
                sampling_event_limit,
            )
            .map_err(|e| format!("drain CPU {} perf ring: {e}", cpu.cpu()))?;
            if outcome.head_consumed {
                if let Some(fence) = loss_fences.get_mut(index) {
                    *fence = now;
                }
            }
            if outcome.raw_full {
                raw_full = true;
                break;
            }
        }
        if raw_full {
            continue;
        }
        if raw_bytes >= NORMAL_RAW_BYTES.saturating_sub(1 << 20) {
            if events.len() < sampling_event_limit {
                append_event(
                    &mut events,
                    &mut raw_bytes,
                    Event {
                        time_ns: now,
                        cpu: 0,
                        sequence: u64::MAX,
                        pid: 0,
                        tid: 0,
                        kind: Kind::Lost {
                            count: 1,
                            reason: b"collector-byte-reservation".to_vec(),
                        },
                    },
                )?;
            }
            raw_full = true;
            continue;
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
    let mut disable_failure_fence = None;
    for (index, cpu) in observation.cpus.iter().enumerate() {
        if !enabled.get(index).copied().unwrap_or(false) {
            continue;
        }
        let disabling = sys::monotonic_ns()
            .map_err(|e| format!("timestamp CPU {} sample disable attempt: {e}", cpu.cpu()))?;
        if let Err(error) = cpu.disable_samples() {
            let message = format!("disable CPU {} samples: {error}", cpu.cpu());
            fatal_errors.push(message);
            let lost_since = loss_fences.get(index).copied().unwrap_or(start_ns);
            disable_failure_fence =
                Some(disable_failure_fence.map_or(lost_since, |prior: u64| prior.min(lost_since)));
            let slot = disable_failed
                .get_mut(index)
                .ok_or("internal: CPU disable-failure index disappeared")?;
            *slot = true;
            let end_slot = coverage_ends
                .get_mut(index)
                .ok_or("internal: CPU failed-disable coverage index disappeared")?;
            *end_slot = Some(disabling);
        } else {
            let stopped = sys::monotonic_ns()
                .map_err(|e| format!("timestamp CPU {} sample disable: {e}", cpu.cpu()))?;
            let slot = coverage_ends
                .get_mut(index)
                .ok_or("internal: CPU coverage end index disappeared")?;
            *slot = Some(stopped);
        }
    }
    for (index, cpu) in observation.cpus.iter_mut().enumerate() {
        if raw_full || disable_failed.get(index).copied().unwrap_or(false) {
            let _ = cpu.drain();
        } else {
            let loss_fence = loss_fences.get(index).copied().unwrap_or(start_ns);
            let outcome = drain_cpu(
                cpu,
                &mut events,
                &mut raw_bytes,
                loss_fence,
                sampling_event_limit,
            )
            .map_err(|e| format!("final drain CPU {} perf ring: {e}", cpu.cpu()))?;
            raw_full = outcome.raw_full;
        }
    }
    if let Some(fence) = disable_failure_fence {
        append_event(
            &mut events,
            &mut raw_bytes,
            Event {
                time_ns: fence,
                cpu: 0,
                sequence: u64::MAX,
                pid: 0,
                tid: 0,
                kind: Kind::Lost {
                    count: disable_failed.iter().filter(|failed| **failed).count() as u64,
                    reason: b"sample-disable-failure".to_vec(),
                },
            },
        )?;
    }
    let end_ns = sys::monotonic_ns().map_err(|e| format!("read capture end: {e}"))?;
    if reporter.is_some() {
        if let Err(error) = finish_report(observation, reporter, false) {
            fatal_errors.push(format!("finish prior capture: {error}"));
            reservation_uncertain = true;
        }
    }
    let carry = std::mem::take(&mut observation.carry);
    let meta = Meta {
        profiler_build: config.profiler_build.clone(),
        deployment: config.deployment.clone(),
        boot_id: boot_id.to_string(),
        start_ns,
        end_ns,
        wall_start_seconds,
        rate_hz: config.rate_hz,
        cpus: observation.cpu_numbers.clone(),
        coverage: observation
            .cpu_numbers
            .iter()
            .copied()
            .zip(coverage_starts.iter().copied())
            .zip(coverage_ends.iter().copied())
            .filter_map(|((cpu, start), end)| Some((cpu, start?, end?)))
            .collect(),
    };
    let mut next_enabled = vec![false; observation.cpus.len()];
    if fatal_errors.is_empty() && !config.once {
        let next_start_ns =
            sys::monotonic_ns().map_err(|e| format!("read next capture start: {e}"))?;
        let next_wall_start_seconds = wall_seconds()?;
        let mut next_starts = vec![None; observation.cpus.len()];
        enable_sampling(
            &observation.cpus,
            &mut next_starts,
            &mut next_enabled,
            &mut fatal_errors,
        )?;
        if fatal_errors.is_empty() && next_enabled.iter().all(|enabled| *enabled) {
            observation.pending_sampling = Some(PendingSampling {
                start_ns: next_start_ns,
                wall_start_seconds: next_wall_start_seconds,
                starts: next_starts,
            });
        }
    }
    match online_cpus() {
        Ok(after) if after == observation.cpu_numbers => {}
        Ok(after) => {
            let message = format!(
                "online CPU topology changed during capture: before {:?}, after {after:?}",
                observation.cpu_numbers
            );
            fatal_errors.push(message);
        }
        Err(error) => {
            let message = format!("read online CPU topology after capture: {error}");
            fatal_errors.push(message);
        }
    }
    if !fatal_errors.is_empty() {
        observation.pending_sampling = None;
        for (cpu, enabled) in observation.cpus.iter().zip(next_enabled) {
            if enabled {
                let _ = cpu.disable_samples();
            }
        }
    }
    if !fatal_errors.is_empty() {
        append_event(
            &mut events,
            &mut raw_bytes,
            terminal_error_event(end_ns, &fatal_errors),
        )?;
    }
    if reservation_uncertain {
        prune(&config.root).map_err(|error| {
            format!(
                "{}; active capture not published because its disk reservation could not be \
                 revalidated: {error}",
                fatal_errors.join("; ")
            )
        })?;
    }
    launch_report(
        observation,
        reporter,
        partial,
        config.root.join(name),
        meta,
        CaptureRosters { events, carry },
        !config.once && fatal_errors.is_empty(),
    )?;
    if fatal_errors.is_empty() {
        Ok(())
    } else {
        finish_report(observation, reporter, true)?;
        Err(fatal_errors.join("; "))
    }
}

fn wall_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "wall clock is before the Unix epoch".to_string())
        .map(|duration| duration.as_secs())
}

fn enable_sampling(
    cpus: &[sys::CpuEvents],
    starts: &mut [Option<u64>],
    enabled: &mut [bool],
    fatal_errors: &mut Vec<String>,
) -> Result<(), String> {
    if starts.len() != cpus.len() || enabled.len() != cpus.len() {
        return Err("internal: CPU sampling roster length mismatch".into());
    }
    for (index, cpu) in cpus.iter().enumerate() {
        let start_slot = starts
            .get_mut(index)
            .ok_or("internal: CPU sampling start slot disappeared")?;
        let enabled_slot = enabled
            .get_mut(index)
            .ok_or("internal: CPU sampling enabled slot disappeared")?;
        let enabling = sys::monotonic_ns()
            .map_err(|e| format!("timestamp CPU {} sample enable: {e}", cpu.cpu()))?;
        match cpu.enable_samples() {
            Ok(()) => {
                *start_slot = Some(enabling);
                *enabled_slot = true;
            }
            Err(error) => {
                let message = format!("enable CPU {} samples: {error}", cpu.cpu());
                fatal_errors.push(message);
            }
        }
    }
    Ok(())
}

fn terminal_error_event(time_ns: u64, errors: &[String]) -> Event {
    let mut message = Vec::new();
    let mut omitted = 0u64;
    for error in errors {
        let separator = usize::from(!message.is_empty()).saturating_mul(2);
        if message
            .len()
            .checked_add(separator)
            .and_then(|length| length.checked_add(error.len()))
            .is_some_and(|length| length <= MAX_TERMINAL_MESSAGE_BYTES)
        {
            if !message.is_empty() {
                message.extend_from_slice(b"; ");
            }
            message.extend_from_slice(error.as_bytes());
        } else {
            omitted = omitted.saturating_add(1);
        }
    }
    if omitted != 0 {
        let suffix = format!("; {omitted} additional terminal errors omitted");
        let remaining = MAX_TERMINAL_MESSAGE_BYTES.saturating_sub(message.len());
        message.extend_from_slice(suffix.as_bytes().get(..remaining).unwrap_or_default());
    }
    error_event(time_ns, &message)
}

fn launch_report(
    observation: &mut Observation,
    reporter: &mut Option<ReportWorker>,
    partial: PathBuf,
    complete: PathBuf,
    meta: Meta,
    rosters: CaptureRosters,
    reserve_next: bool,
) -> Result<(), String> {
    if reporter.is_some() {
        return Err("internal: prior report worker was not reaped".into());
    }
    let root = partial
        .parent()
        .ok_or_else(|| format!("partial capture {} has no root", partial.display()))?
        .to_path_buf();
    let symbolizer = observation
        .symbolizer
        .take()
        .ok_or("internal: report symbolizer is already in use")?;
    let failure_time_ns = meta.end_ns;
    let job = ReportJob {
        meta,
        rosters,
        symbolizer,
    };
    let thread_partial = partial.clone();
    let thread_complete = complete.clone();
    let thread_root = root.clone();
    let (sender, receiver) = mpsc::channel::<ReportJob>();
    let handle = match thread::Builder::new()
        .name("td-profiler-report".into())
        .spawn(move || {
            let mut job = match receiver.recv() {
                Ok(job) => job,
                Err(error) => {
                    let failure = format!("receive report job: {error}");
                    return WorkerOutput {
                        symbolizer: Symbolizer::unavailable(failure.clone()),
                        carry: invalid_carry(failure_time_ns, &failure),
                        result: Err(failure),
                        reservation: Ok(()),
                    };
                }
            };
            let output = process_capture(
                &thread_partial,
                &thread_complete,
                &thread_root,
                &job.meta,
                job.rosters.events,
                job.rosters.carry,
                &mut job.symbolizer,
            );
            let reservation = if reserve_next {
                reserve_next_capture(
                    &thread_root,
                    thread_partial.file_name(),
                    thread_complete.file_name(),
                )
            } else {
                Ok(())
            };
            WorkerOutput {
                symbolizer: job.symbolizer,
                carry: output.carry,
                result: output.result,
                reservation,
            }
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let failure = format!("start report worker: {error}");
            return recover_report_job(observation, &partial, &complete, &root, job, failure);
        }
    };
    if let Err(error) = sender.send(job) {
        let _ = handle.join();
        return recover_report_job(
            observation,
            &partial,
            &complete,
            &root,
            error.0,
            "send report job to worker: receiver disappeared".into(),
        );
    }
    *reporter = Some(ReportWorker {
        partial,
        complete,
        root,
        failure_time_ns,
        handle,
    });
    Ok(())
}

struct ReportJob {
    meta: Meta,
    rosters: CaptureRosters,
    symbolizer: Symbolizer,
}

struct CaptureRosters {
    events: Vec<Event>,
    carry: Vec<Event>,
}

fn recover_report_job(
    observation: &mut Observation,
    partial: &Path,
    complete: &Path,
    root: &Path,
    mut job: ReportJob,
    failure: String,
) -> Result<(), String> {
    if observation.pending_sampling.take().is_some() {
        for cpu in &observation.cpus {
            let _ = cpu.disable_samples();
        }
    }
    let output = process_capture(
        partial,
        complete,
        root,
        &job.meta,
        job.rosters.events,
        job.rosters.carry,
        &mut job.symbolizer,
    );
    observation.symbolizer = Some(job.symbolizer);
    observation.carry = output.carry;
    match output.result {
        Ok(()) => Err(failure),
        Err(processing) => Err(format!("{failure}; {processing}")),
    }
}

fn process_capture(
    partial: &Path,
    complete: &Path,
    root: &Path,
    meta: &Meta,
    mut events: Vec<Event>,
    mut carry: Vec<Event>,
    symbolizer: &mut Symbolizer,
) -> ProcessOutput {
    events.append(&mut carry);
    if events.len() > raw::MAX_DECODED_EVENTS {
        return ProcessOutput::failed(
            meta.end_ns,
            "carry state exceeds the raw decoded-event budget".into(),
        );
    }
    events.sort_by_key(Event::ordering_key);
    if let Err(error) = prepare_partial(partial, root) {
        return ProcessOutput::failed(meta.end_ns, error);
    }
    if let Err(error) = write_raw_capture(partial, &events, meta.end_ns) {
        let failure = format!("write raw capture: {error}");
        return ProcessOutput::published_failure(partial, complete, root, meta.end_ns, failure);
    }
    if let Err(error) = sync_dir(partial) {
        let failure = format!("make raw capture durable: {error}");
        return ProcessOutput::published_failure(partial, complete, root, meta.end_ns, failure);
    }
    if let Err(error) = report::write_pending_manifest(partial, meta, None) {
        let failure = format!("write raw-capture manifest: {error}");
        return ProcessOutput::published_failure(partial, complete, root, meta.end_ns, failure);
    }
    if let Err(error) = sync_dir(partial) {
        let failure = format!("make raw manifest durable: {error}");
        return ProcessOutput::published_failure(partial, complete, root, meta.end_ns, failure);
    }
    let mut analysis = match crate::state::analyze(&events) {
        Ok(analysis) => analysis,
        Err(error) => {
            let failure = format!("analyze raw capture: {error}");
            return ProcessOutput::published_failure(partial, complete, root, meta.end_ns, failure);
        }
    };
    let carry = std::mem::take(&mut analysis.carry);
    if let Err(error) = report::write_pending_manifest(partial, meta, Some(&analysis)) {
        let failure = format!("write analyzed-capture manifest: {error}");
        return ProcessOutput {
            carry,
            result: publish_processing_failure(partial, complete, root, &failure),
        };
    }
    drop(events);
    let generated = report::generate(partial, meta, symbolizer, &analysis, 0);
    ProcessOutput {
        carry,
        result: publish_report_result(partial, complete, root, generated),
    }
}

struct ProcessOutput {
    carry: Vec<Event>,
    result: Result<(), String>,
}

impl ProcessOutput {
    fn failed(time_ns: u64, failure: String) -> Self {
        Self {
            carry: invalid_carry(time_ns, &failure),
            result: Err(failure),
        }
    }

    fn published_failure(
        partial: &Path,
        complete: &Path,
        root: &Path,
        time_ns: u64,
        failure: String,
    ) -> Self {
        Self {
            carry: invalid_carry(time_ns, &failure),
            result: publish_processing_failure(partial, complete, root, &failure),
        }
    }
}

fn invalid_carry(time_ns: u64, failure: &str) -> Vec<Event> {
    let mut reason = b"prior-capture-processing-failure:".to_vec();
    let remaining = MAX_TERMINAL_MESSAGE_BYTES.saturating_sub(reason.len());
    reason.extend_from_slice(
        failure
            .as_bytes()
            .get(..failure.len().min(remaining))
            .unwrap_or_default(),
    );
    vec![Event {
        time_ns,
        cpu: 0,
        sequence: u64::MAX,
        pid: 0,
        tid: 0,
        kind: Kind::Lost { count: 1, reason },
    }]
}

fn prepare_partial(partial: &Path, root: &Path) -> Result<(), String> {
    fs::create_dir(partial)
        .map_err(|e| format!("create partial capture {}: {e}", partial.display()))?;
    fs::set_permissions(partial, fs::Permissions::from_mode(0o2750))
        .map_err(|e| format!("chmod partial capture {}: {e}", partial.display()))?;
    sync_dir(root)
}

fn write_raw_capture(capture: &Path, events: &[Event], end_ns: u64) -> Result<(), String> {
    let raw_path = capture.join("samples.bin");
    let raw_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o640)
        .open(&raw_path)
        .map_err(|e| format!("create {}: {e}", raw_path.display()))?;
    raw_file
        .set_permissions(fs::Permissions::from_mode(0o640))
        .map_err(|e| format!("chmod {}: {e}", raw_path.display()))?;
    let mut writer = raw::Writer::new(BufWriter::new(raw_file)).map_err(|e| e.to_string())?;
    for event in events {
        if !writer.write_event(event).map_err(|e| e.to_string())? {
            let lost = Event {
                time_ns: end_ns,
                cpu: 0,
                sequence: u64::MAX,
                pid: 0,
                tid: 0,
                kind: Kind::Lost {
                    count: 1,
                    reason: b"raw-byte-budget".to_vec(),
                },
            };
            let _ = writer.write_event(&lost).map_err(|e| e.to_string())?;
            break;
        }
    }
    let mut raw_file = writer.into_inner();
    raw_file
        .flush()
        .map_err(|e| format!("flush samples.bin: {e}"))?;
    raw_file
        .get_ref()
        .sync_all()
        .map_err(|e| format!("fsync samples.bin: {e}"))
}

fn publish_processing_failure(
    partial: &Path,
    complete: &Path,
    root: &Path,
    failure: &str,
) -> Result<(), String> {
    match publish_failed_capture(partial, complete, root, failure) {
        Ok(()) => Err(format!(
            "capture processing failed; evidence retained in {}: {failure}",
            complete.display()
        )),
        Err(publish) => Err(format!("{failure}; {publish}")),
    }
}

fn finish_report(
    observation: &mut Observation,
    reporter: &mut Option<ReportWorker>,
    strict: bool,
) -> Result<(), String> {
    let Some(worker) = reporter.take() else {
        return Ok(());
    };
    let ReportWorker {
        partial,
        complete,
        root,
        failure_time_ns,
        handle,
    } = worker;
    let output = match handle.join() {
        Ok(result) => result,
        Err(_) => {
            let failure = "report worker panicked".to_string();
            let published = publish_failed_capture(&partial, &complete, &root, &failure);
            observation.symbolizer = Some(Symbolizer::unavailable(failure.clone()));
            observation.carry = invalid_carry(failure_time_ns, &failure);
            if let Err(error) = published {
                return Err(format!("{failure}; {error}"));
            }
            return Err(failure);
        }
    };
    observation.symbolizer = Some(output.symbolizer);
    observation.carry = output.carry;
    if let Err(reservation) = output.reservation {
        return match output.result {
            Ok(()) => Err(format!("reserve next capture: {reservation}")),
            Err(processing) => Err(format!("{processing}; reserve next capture: {reservation}")),
        };
    }
    match output.result {
        Ok(()) => Ok(()),
        Err(error) if strict => Err(error),
        Err(error) => {
            eprintln!("td-profiler: {error}");
            Ok(())
        }
    }
}

fn publish_report_result(
    partial: &Path,
    complete: &Path,
    root: &Path,
    generated: Result<(), String>,
) -> Result<(), String> {
    let failure = generated.err();
    let mut errors = Vec::new();
    if let Some(error) = failure.as_deref() {
        errors.push(format!("derived report: {error}"));
        if let Err(cleanup) = report::discard_staged_manifest(partial) {
            errors.push(cleanup);
            return Err(errors.join("; "));
        }
        if let Err(marker) = write_failure_marker(partial, error) {
            errors.push(marker);
        }
    }
    if let Err(error) = sync_dir(partial) {
        errors.push(error);
    }
    match fs::rename(partial, complete) {
        Ok(()) => {
            if let Err(error) = sync_dir(root) {
                errors.push(error);
            }
        }
        Err(error) => errors.push(format!(
            "publish capture {} -> {}: {e}",
            partial.display(),
            complete.display(),
            e = error
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn publish_failed_capture(
    partial: &Path,
    complete: &Path,
    root: &Path,
    error: &str,
) -> Result<(), String> {
    report::discard_staged_manifest(partial)?;
    let mut errors = Vec::new();
    if let Err(marker) = write_failure_marker(partial, error) {
        errors.push(marker);
    }
    if let Err(sync) = sync_dir(partial) {
        errors.push(sync);
    }
    match fs::rename(partial, complete) {
        Ok(()) => {
            if let Err(sync) = sync_dir(root) {
                errors.push(sync);
            }
        }
        Err(rename) => errors.push(format!(
            "publish failed capture {} -> {}: {rename}",
            partial.display(),
            complete.display()
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn write_failure_marker(capture: &Path, error: &str) -> Result<(), String> {
    let max_message_bytes = usize::try_from(report::MAX_FAILURE_MARKER_BYTES)
        .unwrap_or(usize::MAX)
        .saturating_sub(1);
    let mut end = error.len().min(max_message_bytes);
    while !error.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let path = capture.join("report-error.txt");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o640))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    file.write_all(
        error
            .as_bytes()
            .get(..end)
            .ok_or("report error marker bound overflow")?,
    )
    .and_then(|()| file.write_all(b"\n"))
    .and_then(|()| file.sync_all())
    .map_err(|e| format!("write {}: {e}", path.display()))
}

fn error_event(time_ns: u64, message: &[u8]) -> Event {
    Event {
        time_ns,
        cpu: 0,
        sequence: u64::MAX,
        pid: 0,
        tid: 0,
        kind: Kind::Error {
            message: message.to_vec(),
        },
    }
}

fn drain_cpu(
    cpu: &mut sys::CpuEvents,
    events: &mut Vec<Event>,
    raw_bytes: &mut u64,
    loss_fence: u64,
    event_limit: usize,
) -> Result<DrainOutcome, String> {
    let number = cpu.cpu();
    let result = cpu.drain();
    let records = match result {
        Ok(records) => records,
        Err(error) => {
            return append_drained(
                events,
                raw_bytes,
                Event {
                    time_ns: loss_fence,
                    cpu: number,
                    sequence: u64::MAX,
                    pid: 0,
                    tid: 0,
                    kind: Kind::Lost {
                        count: 1,
                        reason: format!("ring-error:{error}").into_bytes(),
                    },
                },
                loss_fence,
                number,
                event_limit,
            )
            .map(|appended| DrainOutcome {
                raw_full: !appended,
                head_consumed: false,
            });
        }
    };
    for (sequence, record) in records {
        let event = match perf::decode(&record, number, sequence) {
            Ok(event) => retime_kernel_loss(event, loss_fence),
            Err(error) => Event {
                time_ns: loss_fence,
                cpu: number,
                sequence,
                pid: 0,
                tid: 0,
                kind: Kind::Error {
                    message: format!("CPU {number}: {error}").into_bytes(),
                },
            },
        };
        if !append_drained(events, raw_bytes, event, loss_fence, number, event_limit)? {
            return Ok(DrainOutcome {
                raw_full: true,
                head_consumed: true,
            });
        }
    }
    Ok(DrainOutcome {
        raw_full: false,
        head_consumed: true,
    })
}

fn retime_kernel_loss(mut event: Event, loss_fence: u64) -> Event {
    if matches!(event.kind, Kind::Lost { .. }) {
        event.time_ns = loss_fence;
    }
    event
}

struct DrainOutcome {
    raw_full: bool,
    head_consumed: bool,
}

fn append_drained(
    events: &mut Vec<Event>,
    raw_bytes: &mut u64,
    event: Event,
    loss_fence: u64,
    cpu: u32,
    event_limit: usize,
) -> Result<bool, String> {
    let loss = Event {
        time_ns: loss_fence,
        cpu,
        sequence: u64::MAX,
        pid: 0,
        tid: 0,
        kind: Kind::Lost {
            count: 1,
            reason: b"raw-byte-budget".to_vec(),
        },
    };
    let event_bytes = u64::try_from(raw::encoded_len(&event).map_err(|e| e.to_string())?)
        .map_err(|_| "raw event length does not fit u64")?;
    let loss_bytes = u64::try_from(raw::encoded_len(&loss).map_err(|e| e.to_string())?)
        .map_err(|_| "raw loss length does not fit u64")?;
    let fits_with_loss_reserve = raw_bytes
        .checked_add(event_bytes)
        .and_then(|value| value.checked_add(loss_bytes))
        .map(|value| value <= NORMAL_RAW_BYTES)
        .unwrap_or(false)
        && events.len().saturating_add(2) <= event_limit;
    if !fits_with_loss_reserve {
        append_event(events, raw_bytes, loss)?;
        return Ok(false);
    }
    append_event(events, raw_bytes, event)?;
    Ok(true)
}

fn append_event(events: &mut Vec<Event>, raw_bytes: &mut u64, event: Event) -> Result<(), String> {
    append_bounded(
        events,
        raw_bytes,
        event,
        raw::MAX_RAW_FILE_BYTES,
        "raw capture",
    )
}

fn append_bounded(
    events: &mut Vec<Event>,
    raw_bytes: &mut u64,
    event: Event,
    limit: u64,
    label: &str,
) -> Result<(), String> {
    let bytes = u64::try_from(raw::encoded_len(&event).map_err(|e| e.to_string())?)
        .map_err(|_| "raw record length does not fit u64")?;
    let next = raw_bytes
        .checked_add(bytes)
        .ok_or_else(|| format!("{label} byte count overflow"))?;
    if next > limit {
        return Err(format!("{label} exceeds {limit} bytes"));
    }
    if events.len() >= raw::MAX_DECODED_EVENTS {
        return Err(format!(
            "{label} exceeds {} decoded events",
            raw::MAX_DECODED_EVENTS
        ));
    }
    *raw_bytes = next;
    events.push(event);
    Ok(())
}

fn snapshot_tasks() -> Result<(Vec<Event>, u64), String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))? {
        let entry = entry.map_err(|e| format!("read /proc entry: {e}"))?;
        let Some((pid, path)) = (|| {
            let name = entry.file_name();
            let text = name.to_str()?;
            let pid = text.parse::<u32>().ok()?;
            Some((pid, entry.path()))
        })() else {
            continue;
        };
        if entries.len() >= MAX_SNAPSHOT_TASKS {
            return Err(format!(
                "startup snapshot exceeds {MAX_SNAPSHOT_TASKS} tasks"
            ));
        }
        entries.push((pid, path));
    }
    entries.sort_by_key(|(pid, _)| *pid);
    let mut events = Vec::new();
    let mut bytes = raw::FILE_HEADER_BYTES as u64;
    let mut mapping_count = 0usize;
    let mut sequence = 0u64;
    for (pid, path) in entries {
        let fence = sys::monotonic_ns().map_err(|e| format!("read task {pid} fence: {e}"))?;
        let first = match read_stat(&path.join("stat")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let task_at = events.len();
        append_bounded(
            &mut events,
            &mut bytes,
            Event {
                time_ns: fence,
                cpu: 0,
                sequence,
                pid,
                tid: pid,
                kind: Kind::Task {
                    start: StartIdentity::ProcTicks(first.0),
                    generation: 0,
                    comm: first.1,
                    valid: true,
                },
            },
            crate::state::MAX_CARRY_BYTES,
            "startup baseline",
        )?;
        sequence = sequence.saturating_add(1);
        let maps = match read_bounded(&path.join("maps"), MAX_PROC_MAPS_BYTES) {
            Ok(map_bytes) => parse_maps(
                &map_bytes,
                pid,
                fence,
                &mut sequence,
                MAX_SNAPSHOT_MAPPINGS.saturating_sub(mapping_count),
            )?,
            Err(_) => {
                if let Some(task) = events.get_mut(task_at) {
                    if let Kind::Task { valid, .. } = &mut task.kind {
                        *valid = false;
                    }
                }
                continue;
            }
        };
        let valid = read_stat(&path.join("stat"))
            .map(|after| after.0 == first.0)
            .unwrap_or(false);
        if !valid {
            if let Some(task) = events.get_mut(task_at) {
                if let Kind::Task { valid, .. } = &mut task.kind {
                    *valid = false;
                }
            }
            continue;
        }
        mapping_count = mapping_count
            .checked_add(maps.len())
            .ok_or("startup mapping count overflow")?;
        if mapping_count > MAX_SNAPSHOT_MAPPINGS {
            return Err(format!(
                "startup snapshot exceeds {MAX_SNAPSHOT_MAPPINGS} mappings"
            ));
        }
        for mapping in maps {
            append_bounded(
                &mut events,
                &mut bytes,
                mapping,
                crate::state::MAX_CARRY_BYTES,
                "startup baseline",
            )?;
        }
    }
    Ok((events, bytes))
}

fn read_stat(path: &Path) -> Result<(u64, Vec<u8>), String> {
    let bytes = read_bounded(path, MAX_PROC_STAT_BYTES)?;
    let close = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or("task stat has no closing comm delimiter")?;
    let open = bytes
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or("task stat has no comm")?;
    let comm = bytes
        .get(open.saturating_add(1)..close)
        .ok_or("task comm bounds")?
        .to_vec();
    let rest = bytes
        .get(close.saturating_add(1)..)
        .ok_or("task stat bounds")?;
    let start = rest
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .nth(19)
        .ok_or("task stat has no start time")?;
    let start = std::str::from_utf8(start)
        .map_err(|_| "task start time is not ASCII")?
        .parse::<u64>()
        .map_err(|_| "task start time is invalid")?;
    Ok((start, comm))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(bytes)
}

fn parse_maps(
    bytes: &[u8],
    pid: u32,
    time_ns: u64,
    sequence: &mut u64,
    limit: usize,
) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut fields = line
            .splitn(6, |byte| *byte == b' ')
            .filter(|field| !field.is_empty());
        let Some(range) = fields.next() else { continue };
        let _permissions = fields.next();
        let Some(offset) = fields.next().and_then(hex_u64) else {
            continue;
        };
        let Some(device) = fields.next() else {
            continue;
        };
        let Some(inode) = fields.next().and_then(decimal_u64) else {
            continue;
        };
        let path = fields
            .next()
            .unwrap_or_default()
            .trim_ascii_start()
            .to_vec();
        let Some((start, end)) = split_range(range) else {
            continue;
        };
        let Some((major, minor)) = split_device(device) else {
            continue;
        };
        if events.len() >= limit {
            return Err(format!(
                "startup snapshot exceeds {MAX_SNAPSHOT_MAPPINGS} mappings"
            ));
        }
        events.push(Event {
            time_ns,
            cpu: 0,
            sequence: *sequence,
            pid,
            tid: pid,
            kind: Kind::Mmap {
                address: start,
                length: end.saturating_sub(start),
                page_offset: offset,
                major,
                minor,
                inode,
                inode_generation: 0,
                path,
                synthetic: true,
            },
        });
        *sequence = sequence.saturating_add(1);
    }
    Ok(events)
}

fn split_range(bytes: &[u8]) -> Option<(u64, u64)> {
    let dash = bytes.iter().position(|byte| *byte == b'-')?;
    let start = hex_u64(bytes.get(..dash)?)?;
    let end = hex_u64(bytes.get(dash.saturating_add(1)..)?)?;
    (start < end).then_some((start, end))
}

fn split_device(bytes: &[u8]) -> Option<(u32, u32)> {
    let colon = bytes.iter().position(|byte| *byte == b':')?;
    let major = u32::try_from(hex_u64(bytes.get(..colon)?)?).ok()?;
    let minor = u32::try_from(hex_u64(bytes.get(colon.saturating_add(1)..)?)?).ok()?;
    Some((major, minor))
}

fn hex_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| u64::from_str_radix(text, 16).ok())
}

fn decimal_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse().ok())
}

fn online_cpus() -> Result<Vec<u32>, String> {
    let text = fs::read_to_string("/sys/devices/system/cpu/online")
        .map_err(|e| format!("read online CPU mask: {e}"))?;
    let cpus = cpuset::parse(&text)?;
    if cpus.len() > report::MAX_PROFILE_CPUS {
        return Err(format!(
            "online CPU roster exceeds {} entries",
            report::MAX_PROFILE_CPUS
        ));
    }
    Ok(cpus)
}

fn perf_event_paranoid() -> Result<i32, String> {
    let text = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .map_err(|e| format!("read kernel.perf_event_paranoid: {e}"))?;
    text.trim()
        .parse()
        .map_err(|_| "kernel.perf_event_paranoid is not a canonical integer".into())
}

fn assert_store_mount() -> Result<(), String> {
    let mountinfo = fs::read("/proc/self/mountinfo")
        .map_err(|e| format!("read mountinfo for /td/store attribution: {e}"))?;
    validate_store_mount(&mountinfo)
}

fn validate_store_mount(mountinfo: &[u8]) -> Result<(), String> {
    let target = b"/td/store";
    let mut best: Option<(usize, Vec<u8>, Vec<u8>)> = None;
    for line in mountinfo
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<&[u8]> = line
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .collect();
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or("mountinfo line has no field separator")?;
        let mountpoint = decode_mountinfo(
            fields
                .get(4)
                .copied()
                .ok_or("mountinfo line has no mount point")?,
        )?;
        if mountpoint != target && path_contains(target, &mountpoint) {
            return Err(format!(
                "/td/store attribution disabled: nested mount {} shadows store identity",
                String::from_utf8_lossy(&mountpoint)
            ));
        }
        if !path_contains(&mountpoint, target) {
            continue;
        }
        let options = fields
            .get(5)
            .copied()
            .ok_or("mountinfo line has no mount options")?;
        let filesystem = fields
            .get(separator.saturating_add(1))
            .copied()
            .ok_or("mountinfo line has no filesystem type")?;
        if best
            .as_ref()
            .map(|(length, _, _)| mountpoint.len() >= *length)
            .unwrap_or(true)
        {
            best = Some((mountpoint.len(), options.to_vec(), filesystem.to_vec()));
        }
    }
    let Some((_, options, filesystem)) = best else {
        return Err("/td/store has no containing mount in /proc/self/mountinfo".into());
    };
    let read_only = options
        .split(|byte| *byte == b',')
        .any(|option| option == b"ro");
    if filesystem != b"erofs" || !read_only {
        return Err(format!(
            "/td/store attribution disabled: containing mount is type {} with options {}",
            String::from_utf8_lossy(&filesystem),
            String::from_utf8_lossy(&options)
        ));
    }
    Ok(())
}

fn path_contains(mountpoint: &[u8], target: &[u8]) -> bool {
    mountpoint == b"/"
        || target == mountpoint
        || (target.starts_with(mountpoint) && target.get(mountpoint.len()).copied() == Some(b'/'))
}

fn decode_mountinfo(field: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(field.len());
    let mut at = 0usize;
    while at < field.len() {
        if field.get(at).copied() != Some(b'\\') {
            out.push(*field.get(at).ok_or("mountinfo field offset overflow")?);
            at = at.saturating_add(1);
            continue;
        }
        let escape = field
            .get(at..at.saturating_add(4))
            .ok_or("truncated mountinfo escape")?;
        let decoded = match escape {
            b"\\040" => b' ',
            b"\\011" => b'\t',
            b"\\012" => b'\n',
            b"\\134" => b'\\',
            _ => return Err("unsupported mountinfo escape".into()),
        };
        out.push(decoded);
        at = at.saturating_add(4);
    }
    Ok(out)
}

fn read_boot_id() -> Result<String, String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|e| format!("read boot ID: {e}"))?;
    let value = value.trim();
    if !valid_boot_id(value) {
        return Err("kernel boot ID is not canonical lowercase UUID text".into());
    }
    Ok(value.into())
}

pub(crate) fn process_owner() -> Result<String, String> {
    Ok(format!(
        "{}.{}.{}",
        read_boot_id()?,
        std::process::id(),
        self_start_ticks()?
    ))
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(at, byte)| {
            if matches!(at, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn self_start_ticks() -> Result<u64, String> {
    read_stat(Path::new("/proc/self/stat")).map(|value| value.0)
}

fn verify_credentials(uid: u32, gid: u32) -> Result<(), String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read credentials after drop: {e}"))?;
    if !credentials_match(&status, uid, gid) {
        return Err("credential readback does not match empty-groups uid/gid drop".into());
    }
    Ok(())
}

fn verify_single_threaded() -> Result<(), String> {
    let expected = std::process::id().to_string();
    let mut count = 0usize;
    for entry in fs::read_dir("/proc/self/task")
        .map_err(|e| format!("read profiler task roster before credential drop: {e}"))?
    {
        let entry = entry.map_err(|e| format!("read profiler task entry: {e}"))?;
        count = count.saturating_add(1);
        if entry.file_name().as_bytes() != expected.as_bytes() {
            return Err(format!(
                "profiler must be single-threaded before credential drop; found task {}",
                entry.file_name().to_string_lossy()
            ));
        }
    }
    if count != 1 {
        return Err(format!(
            "profiler must have exactly one task before credential drop; found {count}"
        ));
    }
    Ok(())
}

fn credentials_match(status: &str, uid: u32, gid: u32) -> bool {
    let uid_line = format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}");
    let gid_line = format!("Gid:\t{gid}\t{gid}\t{gid}\t{gid}");
    status.lines().any(|line| line == uid_line)
        && status.lines().any(|line| line == gid_line)
        && status.lines().any(|line| {
            line.strip_prefix("Groups:\t")
                .is_some_and(|groups| groups.trim().is_empty())
        })
}

fn validate_capture_root(root: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|e| format!("stat provisioned capture root {}: {e}", root.display()))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.gid() != gid || mode != 0o2750 {
        return Err(format!(
            "capture root {} must be a directory owned by {uid}:{gid} with mode 02750; got {}:{} mode {mode:04o}",
            root.display(),
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

fn validate_config(config: &Config) -> Result<(), String> {
    if !config.root.is_absolute() {
        return Err("capture root must be absolute".into());
    }
    if config.uid == 0 || config.gid == 0 {
        return Err("profiler uid and gid must be dedicated nonzero identities".into());
    }
    if config.deployment.is_empty() || config.profiler_build.is_empty() {
        return Err("deployment and profiler build identities must be nonempty".into());
    }
    report::validate_identity_metadata(&config.profiler_build, &config.deployment)?;
    if config.rate_hz == 0 || config.rate_hz > 10_000 {
        return Err("sample rate must be in 1..=10000 Hz".into());
    }
    if config.duration.is_zero() || config.duration > Duration::from_secs(86_400) {
        return Err("capture duration must be in 1ns..=1d".into());
    }
    Ok(())
}

fn recover_partials(root: &Path) -> Result<(), String> {
    let boot = read_boot_id()?;
    let root_directory = open_directory(root)?;
    let root_mount = mount_id(&root_directory)?;
    let root_device = root_directory
        .metadata()
        .map_err(|e| format!("stat capture root {}: {e}", root.display()))?
        .dev();
    for name in directory_names(&root_directory)? {
        let bytes = name.as_bytes();
        if !bytes.ends_with(b".partial") {
            continue;
        }
        let text = match name.to_str() {
            Some(text) => text,
            None => {
                quarantine(&root_directory, &name, root_mount, root_device)?;
                continue;
            }
        };
        let Some((owner_boot, pid, start, _sequence)) = partial_owner(text) else {
            quarantine(&root_directory, &name, root_mount, root_device)?;
            continue;
        };
        let stale_owner = read_stat(&PathBuf::from(format!("/proc/{pid}/stat")))
            .map(|value| value.0 != start)
            .unwrap_or(true);
        let stale = owner_boot != boot || stale_owner;
        if stale {
            quarantine(&root_directory, &name, root_mount, root_device)?;
        } else {
            return Err(format!(
                "another td-profiler owns live partial capture {text}"
            ));
        }
    }
    sync_dir(root)
}

fn partial_owner(text: &str) -> Option<(&str, u32, u64, u64)> {
    let fields: Vec<&str> = text.strip_suffix(".partial")?.rsplitn(4, '.').collect();
    if fields.len() != 4 {
        return None;
    }
    let boot = *fields.get(3)?;
    if !valid_boot_id(boot) {
        return None;
    }
    let pid = u32::try_from(canonical_decimal(fields.get(2)?)?).ok()?;
    let start = canonical_decimal(fields.get(1)?)?;
    let sequence = canonical_decimal(fields.first()?)?;
    (pid != 0 && start != 0).then_some((boot, pid, start, sequence))
}

fn canonical_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

fn quarantine(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<(), String> {
    let mut target_name = name.to_os_string();
    target_name.push(".quarantine");
    let path = descriptor_path(parent).join(name);
    let target = descriptor_path(parent).join(target_name);
    match fs::symlink_metadata(&target) {
        Ok(_) => remove_entry(
            parent,
            target
                .file_name()
                .ok_or("quarantine target lost its name")?,
            root_mount,
            root_device,
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "stat quarantine target {}: {error}",
                target.display()
            ));
        }
    }
    fs::rename(&path, &target).map_err(|e| {
        format!(
            "quarantine malformed partial {} -> {}: {e}",
            path.display(),
            target.display()
        )
    })
}

fn prune(root: &Path) -> Result<(), String> {
    prune_preserving(root, None, None)
}

fn reserve_next_capture(
    root: &Path,
    protected_partial: Option<&OsStr>,
    protected_complete: Option<&OsStr>,
) -> Result<(), String> {
    loop {
        match prune_preserving(root, protected_partial, protected_complete) {
            Err(error) if error.starts_with("capture reservation would exceed ") => {
                thread::sleep(Duration::from_millis(100));
            }
            result => return result,
        }
    }
}

fn prune_preserving(
    root: &Path,
    protected_partial: Option<&OsStr>,
    protected_complete: Option<&OsStr>,
) -> Result<(), String> {
    let retention_lock = RetentionLock::acquire(root)?;
    let result = prune_locked(root, protected_partial, protected_complete);
    let release = retention_lock.release();
    match (result, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(release)) => Err(format!("{error}; {release}")),
    }
}

fn prune_locked(
    root: &Path,
    protected_partial: Option<&OsStr>,
    protected_complete: Option<&OsStr>,
) -> Result<(), String> {
    let root_directory = open_directory(root)?;
    let root_metadata = root_directory
        .metadata()
        .map_err(|e| format!("stat capture root {}: {e}", root.display()))?;
    let root_mount = mount_id(&root_directory)?;
    let root_device = root_metadata.dev();
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut reserved_count = 0usize;
    for name in directory_names(&root_directory)? {
        if name == OsStr::new(COLLECTOR_LOCK) || name == OsStr::new(RETENTION_LOCK) {
            continue;
        }
        let bytes = name.as_bytes();
        let complete = !bytes.ends_with(b".partial") && !bytes.ends_with(b".quarantine");
        if complete && regeneration_active(&root_directory, &name, root_mount, root_device)? {
            total = total
                .checked_add(MAX_CAPTURE_BYTES)
                .ok_or("capture retention byte count overflow")?;
            reserved_count = reserved_count.saturating_add(1);
            continue;
        }
        let size = retained_entry_bytes(&root_directory, &name, root_mount, root_device)?;
        let modified = entry_modified(&root_directory, &name, root_mount, root_device)?;
        total = total
            .checked_add(size)
            .ok_or("capture retention byte count overflow")?;
        let protected = protected_partial == Some(name.as_os_str())
            || protected_complete == Some(name.as_os_str());
        entries.push((modified, name, size, complete, protected));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut count = entries.len().saturating_add(reserved_count);
    let mut complete_count = entries.iter().filter(|entry| entry.3).count();
    for (_, name, size, complete, protected) in entries {
        if count < MAX_CAPTURE_COUNT
            && total
                .checked_add(MAX_CAPTURE_BYTES)
                .map(|reserved| reserved <= MAX_TOTAL_BYTES)
                .unwrap_or(false)
        {
            break;
        }
        if protected {
            continue;
        }
        if complete && complete_count <= 1 {
            continue;
        }
        remove_entry(&root_directory, &name, root_mount, root_device)?;
        count = count.saturating_sub(1);
        total = total.saturating_sub(size);
        if complete {
            complete_count = complete_count.saturating_sub(1);
        }
    }
    let reserved = total.checked_add(MAX_CAPTURE_BYTES);
    if count >= MAX_CAPTURE_COUNT
        || reserved
            .map(|reserved| reserved > MAX_TOTAL_BYTES)
            .unwrap_or(true)
    {
        return Err(format!(
            "capture reservation would exceed {MAX_CAPTURE_COUNT} entries or {MAX_TOTAL_BYTES} bytes"
        ));
    }
    sync_dir(root)
}

fn regeneration_active(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<bool, String> {
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat regeneration candidate {}: {e}", path.display()))?;
    if !observed.is_dir() || observed.file_type().is_symlink() {
        return Ok(false);
    }
    let directory = open_directory(&path)?;
    let metadata = verify_child(&directory, &observed, root_mount, root_device, &path)?;
    if !metadata.is_dir() {
        return Err(format!(
            "regeneration candidate changed type: {}",
            path.display()
        ));
    }
    let lock_path = descriptor_path(&directory).join("regenerated.lock");
    let lock_observed = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "stat regeneration lock {}: {error}",
                lock_path.display()
            ));
        }
    };
    if !lock_observed.is_file() || lock_observed.file_type().is_symlink() {
        return Err(format!(
            "regeneration lock is not a regular file: {}",
            lock_path.display()
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        .open(&lock_path)
        .map_err(|e| format!("open regeneration lock {}: {e}", lock_path.display()))?;
    let lock_metadata = verify_child(&lock, &lock_observed, root_mount, root_device, &lock_path)?;
    if !lock_metadata.is_file() {
        return Err(format!(
            "regeneration lock changed type: {}",
            lock_path.display()
        ));
    }
    match lock.try_lock() {
        Ok(()) => {
            lock.unlock()
                .map_err(|e| format!("unlock regeneration probe {}: {e}", lock_path.display()))?;
            Ok(false)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => Err(format!(
            "probe regeneration lock {}: {error}",
            lock_path.display()
        )),
    }
}

pub(crate) fn assert_regeneration_reserved(capture: &Path) -> Result<(), String> {
    let root = capture
        .parent()
        .ok_or_else(|| format!("capture {} has no retention root", capture.display()))?;
    let capture_name = capture
        .file_name()
        .ok_or_else(|| format!("capture {} has no directory name", capture.display()))?;
    let root_directory = open_directory(root)?;
    let root_metadata = root_directory
        .metadata()
        .map_err(|e| format!("stat capture root {}: {e}", root.display()))?;
    let root_mount = mount_id(&root_directory)?;
    let root_device = root_metadata.dev();
    let mut found = false;
    let mut total = 0u64;
    for name in directory_names(&root_directory)? {
        if name == OsStr::new(COLLECTOR_LOCK) || name == OsStr::new(RETENTION_LOCK) {
            continue;
        }
        found |= name == capture_name;
        total = total
            .checked_add(retained_entry_bytes(
                &root_directory,
                &name,
                root_mount,
                root_device,
            )?)
            .ok_or("capture retention reservation overflow")?;
    }
    if !found {
        return Err(format!(
            "capture {} is not a direct retention-root entry",
            capture.display()
        ));
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "regenerated report reservation would exceed {MAX_TOTAL_BYTES} bytes"
        ));
    }
    Ok(())
}

fn retained_entry_bytes(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<u64, String> {
    let bytes = name.as_bytes();
    if bytes.ends_with(b".partial") {
        let size = entry_size(parent, name, root_mount, root_device)?;
        return Ok(size.max(MAX_CAPTURE_BYTES));
    }
    if bytes.ends_with(b".quarantine") {
        return entry_size(parent, name, root_mount, root_device);
    }
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat completed capture {}: {e}", path.display()))?;
    if !observed.is_dir() || observed.file_type().is_symlink() {
        return Err(format!(
            "completed capture is not a directory: {}",
            path.display()
        ));
    }
    let directory = open_directory(&path)?;
    let metadata = verify_child(&directory, &observed, root_mount, root_device, &path)?;
    if !metadata.is_dir() {
        return Err(format!(
            "completed capture changed type: {}",
            path.display()
        ));
    }
    let names = directory_names(&directory)?;
    let mut base = 0u64;
    let mut partial = 0u64;
    let mut lock = 0u64;
    let mut regenerated = false;
    for child in names {
        let child_size = entry_size(&directory, &child, root_mount, root_device)?;
        match child.as_bytes() {
            b"regenerated.partial" => {
                partial = partial
                    .checked_add(child_size)
                    .ok_or("regenerated staging byte count overflow")?;
            }
            b"regenerated.lock" => lock = child_size,
            b"regenerated" => {
                validate_regenerated_directory(&directory, &child, root_mount, root_device)?;
                regenerated = true;
                base = base
                    .checked_add(child_size)
                    .ok_or("completed capture byte count overflow")?;
            }
            _ => {
                base = base
                    .checked_add(child_size)
                    .ok_or("completed capture byte count overflow")?;
            }
        }
    }
    if regenerated {
        if partial != 0 {
            return Err("published and staging regenerated reports coexist".into());
        }
        return base
            .checked_add(lock)
            .ok_or("completed capture byte count overflow".into());
    }
    let staging = partial
        .checked_add(lock)
        .ok_or("regenerated staging byte count overflow")?;
    base.checked_add(staging.max(report::MAX_REPORT_BYTES))
        .ok_or("regenerated report reservation overflow".into())
}

fn validate_regenerated_directory(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<(), String> {
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat regenerated report {}: {e}", path.display()))?;
    if !observed.is_dir() || observed.file_type().is_symlink() {
        return Err(format!(
            "regenerated report is not a directory: {}",
            path.display()
        ));
    }
    let opened = open_directory(&path)?;
    let metadata = verify_child(&opened, &observed, root_mount, root_device, &path)?;
    if !metadata.is_dir() {
        return Err(format!(
            "regenerated report changed type: {}",
            path.display()
        ));
    }
    Ok(())
}

fn open_directory(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        .open(path)
        .map_err(|e| {
            format!(
                "open directory {} without following links: {e}",
                path.display()
            )
        })
}

fn open_regular(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("open file {} without following links: {e}", path.display()))
}

fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn directory_names(directory: &File) -> Result<Vec<std::ffi::OsString>, String> {
    let path = descriptor_path(directory);
    let mut names: Vec<_> = fs::read_dir(&path)
        .map_err(|e| format!("read pinned directory {}: {e}", path.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read pinned directory entry: {e}"))?;
    names.sort();
    Ok(names)
}

fn mount_id(file: &File) -> Result<u64, String> {
    let path = PathBuf::from(format!("/proc/self/fdinfo/{}", file.as_raw_fd()));
    let info = fs::read_to_string(&path)
        .map_err(|e| format!("read descriptor mount identity {}: {e}", path.display()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("descriptor info {} has no mount identity", path.display()))
}

fn verify_child(
    opened: &File,
    observed: &fs::Metadata,
    root_mount: u64,
    root_device: u64,
    path: &Path,
) -> Result<fs::Metadata, String> {
    let metadata = opened
        .metadata()
        .map_err(|e| format!("stat opened capture entry {}: {e}", path.display()))?;
    if metadata.dev() != observed.dev() || metadata.ino() != observed.ino() {
        return Err(format!(
            "capture entry changed while opening: {}",
            path.display()
        ));
    }
    if metadata.dev() != root_device || mount_id(opened)? != root_mount {
        return Err(format!(
            "capture entry crosses a mount boundary: {}",
            path.display()
        ));
    }
    Ok(metadata)
}

fn entry_size(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<u64, String> {
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat capture entry {}: {e}", path.display()))?;
    if observed.file_type().is_symlink() {
        return Ok(observed.len());
    }
    if observed.is_file() {
        let file = open_regular(&path)?;
        let metadata = verify_child(&file, &observed, root_mount, root_device, &path)?;
        if !metadata.is_file() {
            return Err(format!("capture entry changed type: {}", path.display()));
        }
        return Ok(metadata.len());
    }
    if !observed.is_dir() {
        return Err(format!(
            "capture entry has unsupported type: {}",
            path.display()
        ));
    }
    let directory = open_directory(&path)?;
    let metadata = verify_child(&directory, &observed, root_mount, root_device, &path)?;
    if !metadata.is_dir() {
        return Err(format!("capture entry changed type: {}", path.display()));
    }
    let mut total = 0u64;
    for child in directory_names(&directory)? {
        total = total
            .checked_add(entry_size(&directory, &child, root_mount, root_device)?)
            .ok_or("capture entry byte count overflow")?;
    }
    Ok(total)
}

fn entry_modified(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<(i64, i64), String> {
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat capture entry {}: {e}", path.display()))?;
    if observed.file_type().is_symlink() {
        return Ok((observed.mtime(), observed.mtime_nsec()));
    }
    let metadata = if observed.is_file() {
        let file = open_regular(&path)?;
        verify_child(&file, &observed, root_mount, root_device, &path)?
    } else if observed.is_dir() {
        let directory = open_directory(&path)?;
        verify_child(&directory, &observed, root_mount, root_device, &path)?
    } else {
        return Err(format!(
            "capture entry has unsupported type: {}",
            path.display()
        ));
    };
    Ok((metadata.mtime(), metadata.mtime_nsec()))
}

fn remove_entry(
    parent: &File,
    name: &OsStr,
    root_mount: u64,
    root_device: u64,
) -> Result<(), String> {
    let path = descriptor_path(parent).join(name);
    let observed = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat removal candidate {}: {e}", path.display()))?;
    if observed.file_type().is_symlink() {
        return fs::remove_file(&path)
            .map_err(|e| format!("unlink capture symlink {}: {e}", path.display()));
    }
    if observed.is_file() {
        let file = open_regular(&path)?;
        let metadata = verify_child(&file, &observed, root_mount, root_device, &path)?;
        if !metadata.is_file() {
            return Err(format!(
                "removal candidate changed type: {}",
                path.display()
            ));
        }
        drop(file);
        return fs::remove_file(&path)
            .map_err(|e| format!("remove capture file {}: {e}", path.display()));
    }
    if !observed.is_dir() {
        return Err(format!(
            "removal candidate has unsupported type: {}",
            path.display()
        ));
    }
    let directory = open_directory(&path)?;
    let metadata = verify_child(&directory, &observed, root_mount, root_device, &path)?;
    if !metadata.is_dir() {
        return Err(format!(
            "removal candidate changed type: {}",
            path.display()
        ));
    }
    for child in directory_names(&directory)? {
        remove_entry(&directory, &child, root_mount, root_device)?;
    }
    drop(directory);
    fs::remove_dir(&path).map_err(|e| format!("remove capture directory {}: {e}", path.display()))
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("fsync directory {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::{
        append_drained, append_event, credentials_match, directory_names, entry_size, mount_id,
        open_directory, parse_maps, partial_owner, prune, prune_preserving, publish_report_result,
        read_stat, remove_entry, retained_entry_bytes, retime_kernel_loss, terminal_error_event,
        validate_capture_root, validate_config, validate_store_mount, write_failure_marker,
        CollectorLock, Config, MAX_CAPTURE_COUNT, MAX_SNAPSHOT_MAPPINGS, NORMAL_RAW_BYTES,
        NORMAL_RAW_EVENTS,
    };
    use crate::event::{Event, Kind, StartIdentity};
    use crate::raw;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn proc_maps_preserve_path_bytes_and_device_identity() {
        let mut sequence = 0;
        let events = parse_maps(
            b"00400000-00401000 r-xp 00001000 08:02 42                 /td/store/x/bin/a\n",
            9,
            7,
            &mut sequence,
            MAX_SNAPSHOT_MAPPINGS,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        let event = events.first().unwrap();
        assert_eq!(event.pid, 9);
        match &event.kind {
            Kind::Mmap {
                major,
                minor,
                inode,
                path,
                synthetic,
                ..
            } => {
                assert_eq!((*major, *minor, *inode), (8, 2, 42));
                assert_eq!(path, b"/td/store/x/bin/a");
                assert!(*synthetic);
            }
            _ => panic!("not a mapping"),
        }
    }

    #[test]
    fn credential_readback_accepts_the_kernels_empty_group_padding() {
        let status = "Uid:\t997\t997\t997\t997\nGid:\t996\t996\t996\t996\nGroups:\t \n";
        assert!(credentials_match(status, 997, 996));
        assert!(!credentials_match(
            "Uid:\t997\t997\t997\t997\nGid:\t996\t996\t996\t996\nGroups:\t10 \n",
            997,
            996
        ));
    }

    #[test]
    fn coverage_uses_outer_enable_and_disable_timestamps() {
        let source = include_str!("collector.rs");
        let capture = source
            .split("fn collect_capture")
            .nth(1)
            .and_then(|body| body.split("fn error_event").next())
            .unwrap();
        assert!(
            capture.find("let enabling =").unwrap() < capture.find("cpu.enable_samples()").unwrap()
        );
        assert!(
            capture.find("cpu.disable_samples()").unwrap() < capture.find("let stopped =").unwrap()
        );
    }

    #[test]
    fn sampling_restarts_before_analysis_and_reports_behind_the_next_capture() {
        let source = include_str!("collector.rs");
        let capture = source
            .split("fn collect_capture")
            .nth(1)
            .and_then(|body| body.split("fn wall_seconds").next())
            .unwrap();
        let processing = source
            .split("fn process_capture")
            .nth(1)
            .and_then(|body| body.split("fn write_raw_capture").next())
            .unwrap();
        let launch = source
            .split("fn launch_report")
            .nth(1)
            .and_then(|body| body.split("fn process_capture").next())
            .unwrap();
        let rotation = capture.split("let end_ns").nth(1).unwrap();
        assert!(
            rotation.find("finish_report(").unwrap()
                < rotation
                    .find("observation.pending_sampling = Some")
                    .unwrap()
        );
        assert!(
            rotation
                .find("observation.pending_sampling = Some")
                .unwrap()
                < rotation.find("launch_report(").unwrap()
        );
        assert!(!rotation[..rotation.find("launch_report(").unwrap()].contains("fs::"));
        assert!(capture.contains("ReportWorker::is_finished"));
        assert!(capture.contains("cpu.drain()"));
        assert!(
            processing.find("prepare_partial").unwrap()
                < processing.find("write_raw_capture").unwrap()
        );
        assert!(
            processing.find("write_raw_capture").unwrap()
                < processing.find("crate::state::analyze").unwrap()
        );
        assert!(
            processing.find("crate::state::analyze").unwrap()
                < processing.find("report::generate").unwrap()
        );
        assert!(
            launch.find("process_capture(").unwrap()
                < launch.find("reserve_next_capture(").unwrap()
        );
    }

    #[test]
    fn collector_root_lock_is_kernel_exclusive_and_persistent() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-collector-lock-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let first = CollectorLock::acquire(&root).unwrap();
        assert!(CollectorLock::acquire(&root)
            .err()
            .unwrap()
            .contains("another td-profiler"));
        prune(&root).unwrap();
        drop(first);
        drop(CollectorLock::acquire(&root).unwrap());
        assert!(root.join("collector.lock").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_derived_report_still_publishes_raw_evidence() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-failed-report-test-{}",
            std::process::id()
        ));
        let partial = root.join("capture.partial");
        let complete = root.join("capture");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&partial).unwrap();
        drop(
            crate::raw::Writer::new(std::fs::File::create(partial.join("samples.bin")).unwrap())
                .unwrap(),
        );
        let raw = std::fs::read(partial.join("samples.bin")).unwrap();
        crate::report::write_pending_manifest(
            &partial,
            &crate::report::Meta {
                profiler_build: "profiler".into(),
                deployment: "deployment".into(),
                boot_id: "boot".into(),
                start_ns: 1,
                end_ns: 2,
                wall_start_seconds: 3,
                rate_hz: 99,
                cpus: vec![0],
                coverage: vec![(0, 1, 2)],
            },
            None,
        )
        .unwrap();
        let error =
            publish_report_result(&partial, &complete, &root, Err("boom".into())).unwrap_err();
        assert!(error.contains("boom"));
        assert_eq!(std::fs::read(complete.join("samples.bin")).unwrap(), raw);
        assert!(std::fs::read_to_string(complete.join("report-error.txt"))
            .unwrap()
            .contains("boom"));
        crate::report::regenerate(&complete, None).unwrap();
        assert!(complete.join("regenerated/manifest.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_root_must_already_match_the_post_drop_identity() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-root-contract-test-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2750)).unwrap();
        let metadata = root.metadata().unwrap();
        validate_capture_root(&root, metadata.uid(), metadata.gid()).unwrap();
        let error = validate_capture_root(&root, metadata.uid().saturating_add(1), metadata.gid())
            .unwrap_err();
        assert!(error.contains("owned by"));
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn proc_maps_stop_at_the_aggregate_snapshot_limit() {
        let mut sequence = 0;
        let error = parse_maps(
            b"00400000-00401000 r-xp 00001000 08:02 42 /td/store/x/bin/a\n",
            9,
            7,
            &mut sequence,
            0,
        )
        .unwrap_err();
        assert!(error.contains("mappings"), "{error}");
    }

    #[test]
    fn raw_budget_replaces_the_first_nonfitting_record_with_loss() {
        const { assert!(crate::state::MAX_CARRY_EVENTS < raw::MAX_DECODED_EVENTS) };
        let loss = Event {
            time_ns: 7,
            cpu: 2,
            sequence: u64::MAX,
            pid: 0,
            tid: 0,
            kind: Kind::Lost {
                count: 1,
                reason: b"raw-byte-budget".to_vec(),
            },
        };
        let loss_bytes = raw::encoded_len(&loss).unwrap() as u64;
        let mut bytes = NORMAL_RAW_BYTES - loss_bytes;
        let mut events = Vec::new();
        let appended = append_drained(
            &mut events,
            &mut bytes,
            Event {
                time_ns: 7,
                cpu: 2,
                sequence: 1,
                pid: 9,
                tid: 9,
                kind: Kind::Task {
                    start: StartIdentity::ProcTicks(1),
                    generation: 0,
                    comm: b"task".to_vec(),
                    valid: true,
                },
            },
            7,
            2,
            NORMAL_RAW_EVENTS,
        )
        .unwrap();
        assert!(!appended);
        assert_eq!(bytes, NORMAL_RAW_BYTES);
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(Kind::Lost { reason, .. }) if reason == b"raw-byte-budget"
        ));
        append_event(
            &mut events,
            &mut bytes,
            terminal_error_event(8, &["disable failed".into()]),
        )
        .unwrap();
        assert!(bytes <= raw::MAX_RAW_FILE_BYTES);
    }

    #[test]
    fn kernel_loss_uses_the_prior_consumed_head_fence() {
        let loss = retime_kernel_loss(
            Event {
                time_ns: 900,
                cpu: 3,
                sequence: 4,
                pid: 0,
                tid: 0,
                kind: Kind::Lost {
                    count: 7,
                    reason: b"kernel".to_vec(),
                },
            },
            500,
        );
        assert_eq!(loss.time_ns, 500);
    }

    #[test]
    fn failure_marker_stays_inside_its_metadata_reservation() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-failure-marker-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        write_failure_marker(&root, &"x".repeat(32 * 1024)).unwrap();
        assert!(
            root.join("report-error.txt").metadata().unwrap().len()
                <= crate::report::MAX_FAILURE_MARKER_BYTES
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_owner_names_require_every_canonical_field() {
        let boot = "12345678-1234-1234-1234-123456789abc";
        assert_eq!(
            partial_owner(&format!("{boot}.9.10.11.partial")),
            Some((boot, 9, 10, 11))
        );
        for invalid in [
            format!("{boot}.9.10.no.partial"),
            format!("{boot}.9.no.11.partial"),
            format!("{boot}.09.10.11.partial"),
            format!("{boot}.9.010.11.partial"),
            format!("{boot}.0.10.11.partial"),
            "not-a-boot.9.10.11.partial".into(),
            format!("{boot}.9.10.partial"),
        ] {
            assert!(partial_owner(&invalid).is_none(), "accepted {invalid}");
        }
    }

    #[test]
    fn self_stat_parser_reads_a_stable_start_time() {
        let (start, comm) = read_stat(Path::new("/proc/self/stat")).unwrap();
        assert!(start > 0);
        assert!(!comm.is_empty());
    }

    #[test]
    fn store_attribution_requires_the_deepest_mount_to_be_read_only_erofs() {
        let root_erofs = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n";
        assert!(validate_store_mount(root_erofs).is_ok());

        let writable_overlay = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n\
33 24 0:33 / /td/store rw,relatime - tmpfs tmpfs rw\n";
        let error = validate_store_mount(writable_overlay).unwrap_err();
        assert!(error.contains("type tmpfs"), "{error}");

        let escaped_sibling = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n\
33 24 0:33 / /td/store\\040copy rw,relatime - tmpfs tmpfs rw\n";
        assert!(validate_store_mount(escaped_sibling).is_ok());

        let stacked = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n\
33 24 0:33 / /td/store ro,relatime - erofs /dev/vdb ro\n\
34 24 0:34 / /td/store rw,relatime - tmpfs tmpfs rw\n";
        assert!(validate_store_mount(stacked).unwrap_err().contains("tmpfs"));

        let visible_erofs = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n\
33 24 0:33 / /td/store rw,relatime - tmpfs tmpfs rw\n\
34 24 0:34 / /td/store ro,relatime - erofs /dev/vdb ro\n";
        assert!(validate_store_mount(visible_erofs).is_ok());

        let nested = b"24 1 0:22 / / ro,relatime - erofs /dev/vda ro\n\
33 24 0:33 / /td/store ro,relatime - erofs /dev/vdb ro\n\
34 33 0:34 / /td/store/injected ro,relatime - erofs /dev/vdc ro\n";
        assert!(validate_store_mount(nested)
            .unwrap_err()
            .contains("nested mount"));
    }

    #[test]
    fn collection_requires_a_dedicated_nonzero_identity() {
        let mut config = Config {
            root: PathBuf::from("/capture"),
            index: None,
            deployment: "deployment".into(),
            profiler_build: "/td/store/profiler".into(),
            uid: 0,
            gid: 100,
            rate_hz: 99,
            duration: Duration::from_secs(1),
            once: true,
        };
        assert!(validate_config(&config).unwrap_err().contains("nonzero"));
        config.uid = 100;
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn retention_walks_from_pinned_descriptors_without_following_symlinks() {
        let root =
            std::env::temp_dir().join(format!("td-profiler-retention-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("capture")).unwrap();
        std::fs::write(root.join("capture/samples.bin"), b"1234").unwrap();
        let directory = open_directory(&root).unwrap();
        let metadata = directory.metadata().unwrap();
        let mount = mount_id(&directory).unwrap();
        assert_eq!(directory_names(&directory).unwrap().len(), 1);
        assert_eq!(
            entry_size(
                &directory,
                std::ffi::OsStr::new("capture"),
                mount,
                metadata.dev()
            )
            .unwrap(),
            4
        );

        std::fs::write(root.join("outside"), b"keep").unwrap();
        std::os::unix::fs::symlink("../outside", root.join("capture/link")).unwrap();
        let link_bytes = std::fs::symlink_metadata(root.join("capture/link"))
            .unwrap()
            .len();
        assert_eq!(
            entry_size(
                &directory,
                std::ffi::OsStr::new("capture"),
                mount,
                metadata.dev()
            )
            .unwrap(),
            4 + link_bytes
        );
        remove_entry(
            &directory,
            std::ffi::OsStr::new("capture"),
            mount,
            metadata.dev(),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("outside")).unwrap(), b"keep");
        std::fs::remove_file(root.join("outside")).unwrap();
        drop(directory);
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn retention_counts_and_reclaims_partial_and_quarantine_entries() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-retention-count-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("only-complete")).unwrap();
        for number in 0..MAX_CAPTURE_COUNT.saturating_sub(1) {
            let suffix = if number % 2 == 0 {
                "partial"
            } else {
                "quarantine"
            };
            std::fs::create_dir(root.join(format!("old-{number:03}.{suffix}"))).unwrap();
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), MAX_CAPTURE_COUNT);
        prune(&root).unwrap();
        assert_eq!(capture_entry_count(&root), 19);
        assert!(root.join("only-complete").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_reserves_regeneration_for_every_completed_capture() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-retention-regeneration-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        const ACTUAL: u64 = 80 * 1024 * 1024;
        for number in 0..14 {
            let capture = root.join(format!("capture-{number:02}"));
            std::fs::create_dir(&capture).unwrap();
            std::fs::File::create(capture.join("samples.bin"))
                .unwrap()
                .set_len(ACTUAL)
                .unwrap();
        }
        prune(&root).unwrap();
        let allowed = (super::MAX_TOTAL_BYTES - super::MAX_CAPTURE_BYTES)
            / (ACTUAL + crate::report::MAX_REPORT_BYTES);
        assert_eq!(capture_entry_count(&root) as u64, allowed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_processing_prune_never_reclaims_the_capture_it_just_published() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-retention-published-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("00-published")).unwrap();
        for number in 1..MAX_CAPTURE_COUNT {
            std::fs::create_dir(root.join(format!("capture-{number:02}"))).unwrap();
        }
        prune_preserving(
            &root,
            Some(std::ffi::OsStr::new("00-published.partial")),
            Some(std::ffi::OsStr::new("00-published")),
        )
        .unwrap();
        assert!(root.join("00-published").is_dir());
        assert_eq!(capture_entry_count(&root), MAX_CAPTURE_COUNT - 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_offline_regeneration_is_reserved_without_blocking_prune() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-retention-regenerating-test-{}",
            std::process::id()
        ));
        let capture = root.join("capture");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&capture).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(capture.join("regenerated.lock"))
            .unwrap();
        lock.lock().unwrap();
        std::fs::write(capture.join("regenerated"), b"mutating").unwrap();
        prune(&root).unwrap();
        assert!(capture.is_dir());
        lock.unlock().unwrap();
        assert!(prune(&root).unwrap_err().contains("not a directory"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regeneration_growth_consumes_its_fixed_allowance_once() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-retention-active-report-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let capture = root.join("capture");
        let partial = capture.join("regenerated.partial");
        std::fs::create_dir_all(&partial).unwrap();
        const BASE: u64 = 80 * 1024 * 1024;
        let samples = std::fs::File::create(capture.join("samples.bin")).unwrap();
        samples.set_len(BASE).unwrap();
        std::fs::write(capture.join("regenerated.lock"), b"owner").unwrap();
        let output = std::fs::File::create(partial.join("stacks.jsonl")).unwrap();
        output.set_len(10 * 1024 * 1024).unwrap();

        let directory = open_directory(&root).unwrap();
        let metadata = directory.metadata().unwrap();
        let mount = mount_id(&directory).unwrap();
        let charged = || {
            retained_entry_bytes(
                &directory,
                std::ffi::OsStr::new("capture"),
                mount,
                metadata.dev(),
            )
            .unwrap()
        };
        assert_eq!(charged(), BASE + crate::report::MAX_REPORT_BYTES);
        output.set_len(63 * 1024 * 1024).unwrap();
        assert_eq!(charged(), BASE + crate::report::MAX_REPORT_BYTES);

        drop(directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn capture_entry_count(root: &Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                !matches!(
                    entry.file_name().as_encoded_bytes(),
                    b"collector.lock" | b"retention.lock"
                )
            })
            .count()
    }
}

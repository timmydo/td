use crate::event::StartIdentity;
use crate::json;
use crate::raw;
use crate::state::{self, Frame, ImageKey, StackState};
use crate::symbol::{resolved_json, Resolved, Symbolizer};
use std::cell::Cell;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const SCHEMA: u32 = 1;
pub const MAX_REPORT_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_OVERVIEW_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_CAPTURE_METADATA_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_FAILURE_MARKER_BYTES: u64 = 16 * 1024;
pub(crate) const MAX_PROFILE_CPUS: usize = 4096;
const FIXED_METADATA_RESERVE_BYTES: u64 = 4096;
const PER_CPU_METADATA_RESERVE_BYTES: u64 = 96;
const MAX_REPORT_EXPANSION_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const OVERVIEW_ROWS_PER_KIND: usize = 32;
pub(crate) const OVERVIEW_FIELD_PREFIX_BYTES: usize = 1024;
const O_NOFOLLOW: i32 = 0o400_000;
const O_CLOEXEC: i32 = 0o2_000_000;

pub struct Meta {
    pub profiler_build: String,
    pub deployment: String,
    pub boot_id: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub wall_start_seconds: u64,
    pub rate_hz: u32,
    pub cpus: Vec<u32>,
    pub coverage: Vec<(u32, u64, u64)>,
}

pub fn validate_identity_metadata(profiler_build: &str, deployment: &str) -> Result<(), String> {
    let identities = json::string(profiler_build.as_bytes())
        .and_then(|profiler| {
            json::string(deployment.as_bytes())
                .and_then(|deployment| profiler.len().checked_add(deployment.len()))
        })
        .ok_or("profile identity metadata expansion overflow")?;
    let cpu_reserve = (MAX_PROFILE_CPUS as u64)
        .checked_mul(PER_CPU_METADATA_RESERVE_BYTES)
        .ok_or("CPU metadata reservation overflow")?;
    let identity_limit = MAX_CAPTURE_METADATA_BYTES
        .checked_sub(MAX_FAILURE_MARKER_BYTES)
        .and_then(|remaining| remaining.checked_sub(FIXED_METADATA_RESERVE_BYTES))
        .and_then(|remaining| remaining.checked_sub(cpu_reserve))
        .ok_or("capture metadata reservation is internally inconsistent")?;
    if identities as u64 > identity_limit {
        return Err(format!(
            "deployment and profiler identities expand beyond {identity_limit} metadata bytes"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HotspotKey {
    image: ImageKey,
    resolved: bool,
    function_address: u64,
    function: Vec<u8>,
    object: Vec<u8>,
    build_id: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LineKey {
    image: ImageKey,
    symbol_resolved: bool,
    line_resolved: bool,
    function_address: u64,
    object_address: u64,
    function: Vec<u8>,
    object: Vec<u8>,
    build_id: Vec<u8>,
    source_file: Vec<u8>,
    source_line: u64,
    source_column: u64,
    discriminator: u64,
}

struct StackRows {
    hotspots: BTreeMap<HotspotKey, u64>,
    lines: BTreeMap<LineKey, u64>,
    unresolved_stacks: u64,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
}

pub fn generate(
    capture: &Path,
    meta: &Meta,
    symbolizer: &mut Symbolizer,
    analysis: &state::Analysis,
    unknown_records: u64,
) -> Result<(), String> {
    let budget =
        OutputBudget::with_limit(MAX_REPORT_BYTES.saturating_sub(MAX_CAPTURE_METADATA_BYTES));
    let mut expansion = ExpansionBudget::new();
    let mut overview = output_bounded(capture.join("overview.jsonl"), &budget, MAX_OVERVIEW_BYTES)?;
    let process_rows = write_processes(capture, analysis, &budget, &mut overview)?;
    let rows = write_stacks(capture, analysis, symbolizer, &budget, &mut expansion)?;
    let hotspot_rows = write_hotspots(capture, rows.hotspots, &budget, &mut overview, analysis)?;
    let line_rows = write_lines(capture, rows.lines, &budget, &mut overview, analysis)?;
    write_overview_summary(
        &mut overview,
        analysis,
        symbolizer,
        process_rows,
        hotspot_rows,
        line_rows,
        rows.complete_stack_samples,
        rows.truncated_stack_samples,
        rows.unresolved_stack_samples,
        rows.line_resolved_samples,
        rows.line_unresolved_samples,
    )?;
    finish(overview, "overview.jsonl")?;
    write_manifest(
        capture,
        meta,
        analysis,
        unknown_records,
        rows.unresolved_stacks,
        rows.complete_stack_samples,
        rows.truncated_stack_samples,
        rows.unresolved_stack_samples,
        rows.line_resolved_samples,
        rows.line_unresolved_samples,
        symbolizer,
        &budget,
        &mut expansion,
    )?;
    Ok(())
}

pub fn write_pending_manifest(
    capture: &Path,
    meta: &Meta,
    analysis: Option<&state::Analysis>,
) -> Result<(), String> {
    let prefix = live_manifest_prefix(meta, analysis.map(|value| value.sample_records))?;
    let budget = OutputBudget::with_limit(
        MAX_CAPTURE_METADATA_BYTES.saturating_sub(MAX_FAILURE_MARKER_BYTES),
    );
    let next = capture.join("manifest.json.next");
    let result = (|| {
        let mut file = output(next.clone(), &budget)?;
        let empty = state::Analysis::default();
        let analysis = analysis.unwrap_or(&empty);
        writeln!(
            file,
            "{},\"samples\":{},\"tasks\":{},\"mappings\":{},\"context_switches\":{},\
             \"lost\":{},\"corrupt\":{},\"ignored_perf_records\":{},\
             \"unknown_raw_records\":0,\"omitted_errors\":{},\"unresolved_stacks\":0,\
             \"complete_stack_samples\":0,\"truncated_stack_samples\":0,\
             \"unresolved_stack_samples\":0,\
             \"line_resolved_samples\":0,\"line_unresolved_samples\":0,\
             \"objects\":[],\"errors\":[],\"report_incomplete\":true}}",
            prefix,
            analysis.sample_records,
            analysis.task_records,
            analysis.mapping_records,
            analysis.switch_records,
            analysis.lost_records,
            analysis.corrupt_records,
            analysis.ignored_records,
            analysis.omitted_errors,
        )
        .map_err(|e| format!("write pending manifest.json: {e}"))?;
        finish(file, "pending manifest.json")?;
        fs::rename(&next, capture.join("manifest.json"))
            .map_err(|e| format!("publish pending manifest.json: {e}"))?;
        sync_dir(capture)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => match discard_staged_manifest(capture) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!("{error}; {cleanup}")),
        },
    }
}

pub(crate) fn discard_staged_manifest(capture: &Path) -> Result<(), String> {
    let next = capture.join("manifest.json.next");
    match fs::remove_file(&next) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove staged manifest {} before failure publication: {error}",
            next.display()
        )),
    }
}

pub fn regenerate(capture: &Path, index: Option<&Path>) -> Result<(), String> {
    if !capture.is_absolute() {
        return Err("report capture path must be absolute".into());
    }
    let retention_root = capture
        .parent()
        .ok_or_else(|| format!("capture {} has no retention root", capture.display()))?;
    let retention_lock = crate::collector::RetentionLock::acquire(retention_root)?;
    let started = (|| {
        let prefix = manifest_prefix(capture)?;
        let complete = capture.join("regenerated");
        let budget = OutputBudget::new();
        let lock = RegenerationLock::acquire(capture, &budget)?;
        match fs::symlink_metadata(&complete) {
            Ok(_) => {
                return Err(format!(
                    "regenerated report already exists: {}",
                    complete.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "stat regenerated report {}: {error}",
                    complete.display()
                ));
            }
        }
        let partial = capture.join("regenerated.partial");
        remove_stale_partial(&partial)?;
        crate::collector::assert_regeneration_reserved(capture)?;
        Ok((prefix, complete, partial, budget, lock))
    })();
    let retention_release = retention_lock.release();
    let (prefix, complete, partial, budget, lock) = match (started, retention_release) {
        (Ok(started), Ok(())) => started,
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(release)) => return Err(format!("{error}; {release}")),
        (Ok((_, _, _, _, lock)), Err(error)) => {
            let release = lock.release();
            return match release {
                Ok(()) => Err(error),
                Err(release) => Err(format!("{error}; {release}")),
            };
        }
    };
    let result = (|| {
        fs::create_dir(&partial)
            .map_err(|e| format!("create regenerated report {}: {e}", partial.display()))?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o2750))
            .map_err(|e| format!("chmod regenerated report {}: {e}", partial.display()))?;
        regenerate_partial(capture, index, &prefix, &partial, &complete, &budget)
    })();
    if let Err(error) = result {
        let cleanup = remove_stale_partial(&partial);
        let release = lock.release();
        let sync = sync_dir(capture);
        let mut errors = vec![error];
        errors.extend(cleanup.err());
        errors.extend(release.err());
        errors.extend(sync.err());
        return Err(errors.join("; "));
    }
    lock.release()?;
    sync_dir(capture)
}

fn regenerate_partial(
    capture: &Path,
    index: Option<&Path>,
    prefix: &str,
    partial: &Path,
    complete: &Path,
    budget: &OutputBudget,
) -> Result<(), String> {
    let mut expansion = ExpansionBudget::new();
    let (analysis, unknown_records) = analyze_capture(capture)?;
    let prefix = regenerated_manifest_prefix(prefix, analysis.sample_records)?;
    let mut symbolizer = Symbolizer::from_index(index);
    let mut overview = output_bounded(partial.join("overview.jsonl"), budget, MAX_OVERVIEW_BYTES)?;
    let process_rows = write_processes(partial, &analysis, budget, &mut overview)?;
    let rows = write_stacks(partial, &analysis, &mut symbolizer, budget, &mut expansion)?;
    let hotspot_rows = write_hotspots(partial, rows.hotspots, budget, &mut overview, &analysis)?;
    let line_rows = write_lines(partial, rows.lines, budget, &mut overview, &analysis)?;
    write_overview_summary(
        &mut overview,
        &analysis,
        &symbolizer,
        process_rows,
        hotspot_rows,
        line_rows,
        rows.complete_stack_samples,
        rows.truncated_stack_samples,
        rows.unresolved_stack_samples,
        rows.line_resolved_samples,
        rows.line_unresolved_samples,
    )?;
    finish(overview, "overview.jsonl")?;
    write_manifest_tail(
        partial,
        &prefix,
        &analysis,
        unknown_records,
        rows.unresolved_stacks,
        rows.complete_stack_samples,
        rows.truncated_stack_samples,
        rows.unresolved_stack_samples,
        rows.line_resolved_samples,
        rows.line_unresolved_samples,
        &symbolizer,
        budget,
        &mut expansion,
        true,
        "manifest.json",
    )?;
    sync_dir(partial)?;
    fs::rename(partial, complete).map_err(|e| {
        format!(
            "publish regenerated report {} -> {}: {e}",
            partial.display(),
            complete.display()
        )
    })
}

struct RegenerationLock {
    file: File,
    path: PathBuf,
}

impl RegenerationLock {
    fn acquire(capture: &Path, budget: &OutputBudget) -> Result<Self, String> {
        let path = capture.join("regenerated.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o640)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC)
            .open(&path)
            .map_err(|e| format!("open regeneration lock {}: {e}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o640))
            .map_err(|e| format!("chmod regeneration lock {}: {e}", path.display()))?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => "regenerated report is already in progress".to_string(),
            fs::TryLockError::Error(error) => {
                format!("lock regenerated report {}: {error}", path.display())
            }
        })?;
        let owner = crate::collector::process_owner()?;
        budget.claim(owner.len() as u64, "regeneration lock")?;
        file.set_len(0)
            .map_err(|e| format!("truncate regeneration lock {}: {e}", path.display()))?;
        file.write_all(owner.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("write regeneration lock {}: {e}", path.display()))?;
        Ok(Self { file, path })
    }

    fn release(self) -> Result<(), String> {
        self.file
            .unlock()
            .map_err(|e| format!("unlock regenerated report {}: {e}", self.path.display()))
    }
}

fn remove_stale_partial(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
                .map_err(|e| format!("remove stale regenerated report {}: {e}", path.display()))
        }
        Ok(_) => Err(format!(
            "stale regenerated report is not a directory: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "stat stale regenerated report {}: {error}",
            path.display()
        )),
    }
}

fn analyze_capture(source: &Path) -> Result<(state::Analysis, u64), String> {
    let samples =
        File::open(source.join("samples.bin")).map_err(|e| format!("open samples.bin: {e}"))?;
    let decoded = raw::read(BufReader::new(samples))?;
    let analysis = state::analyze(&decoded.events)?;
    Ok((analysis, decoded.unknown_records))
}

fn manifest_prefix(capture: &Path) -> Result<String, String> {
    let path = capture.join("manifest.json");
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?
        .take(MAX_REPORT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(format!(
            "{} exceeds {MAX_REPORT_BYTES} bytes",
            path.display()
        ));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8 JSON", path.display()))?;
    if text.lines().count() != 1 || !text.starts_with("{\"schema\":1,\"profiler_build\":") {
        return Err("existing manifest is not canonical td-profiler schema 1".into());
    }
    let marker = ",\"samples\":";
    let at = text
        .find(marker)
        .ok_or("existing manifest has no canonical derived-field boundary")?;
    if text
        .get(at.saturating_add(marker.len())..)
        .is_some_and(|tail| tail.contains(marker))
    {
        return Err("existing manifest repeats its derived-field boundary".into());
    }
    let stable = text.get(..at).ok_or("existing manifest prefix overflow")?;
    let mut after = 0usize;
    for field in [
        "\"profiler_build\":",
        "\"deployment\":",
        "\"boot_id\":",
        "\"monotonic_start_ns\":",
        "\"monotonic_end_ns\":",
        "\"wall_start_seconds\":",
        "\"configured_rate_hz\":",
        "\"coverage_duration_ns\":",
        "\"effective_rate_millihz\":",
        "\"cpus\":",
        "\"coverage\":",
    ] {
        let found = stable
            .get(after..)
            .and_then(|tail| tail.find(field))
            .ok_or_else(|| format!("existing manifest lacks canonical field {field}"))?;
        after = after
            .checked_add(found)
            .and_then(|value| value.checked_add(field.len()))
            .ok_or("existing manifest field offset overflow")?;
    }
    Ok(stable.to_string())
}

fn regenerated_manifest_prefix(prefix: &str, samples: u64) -> Result<String, String> {
    let coverage_label = "\"coverage_duration_ns\":";
    let coverage_at = prefix
        .rfind(coverage_label)
        .and_then(|at| at.checked_add(coverage_label.len()))
        .ok_or("existing manifest has no coverage duration")?;
    let coverage_end = prefix
        .get(coverage_at..)
        .and_then(|tail| tail.find(','))
        .and_then(|length| coverage_at.checked_add(length))
        .ok_or("existing manifest coverage duration is not bounded")?;
    let coverage_ns: u128 = prefix
        .get(coverage_at..coverage_end)
        .ok_or("existing manifest coverage duration overflow")?
        .parse()
        .map_err(|_| "existing manifest coverage duration is not canonical")?;

    let rate_label = "\"effective_rate_millihz\":";
    let rate_at = prefix
        .rfind(rate_label)
        .and_then(|at| at.checked_add(rate_label.len()))
        .ok_or("existing manifest has no effective rate")?;
    let rate_end = prefix
        .get(rate_at..)
        .and_then(|tail| tail.find(','))
        .and_then(|length| rate_at.checked_add(length))
        .ok_or("existing manifest effective rate is not bounded")?;
    let rate = effective_rate_for_duration(samples, coverage_ns)
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let mut rebuilt = prefix.to_string();
    rebuilt.replace_range(rate_at..rate_end, &rate);
    Ok(rebuilt)
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| format!("fsync directory {}: {e}", path.display()))
}

fn write_processes(
    capture: &Path,
    analysis: &state::Analysis,
    budget: &OutputBudget,
    overview: &mut Output,
) -> Result<usize, String> {
    let mut processes: Vec<_> = analysis.processes.values().collect();
    processes.sort_by(|left, right| {
        right
            .samples
            .cmp(&left.samples)
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut file = output(capture.join("processes.jsonl"), budget)?;
    let row_count = processes.len();
    for (index, process) in processes.into_iter().enumerate() {
        let (start_kind, start_value) = start_identity(&process.key.start);
        writeln!(
            file,
            "{{\"schema\":{SCHEMA},\"pid\":{},\"start_kind\":\"{}\",\"start_value\":{},\
             \"generation\":{},\
             {},\"observed\":{},\"baseline_valid\":{},\"exited\":{},\"samples\":{}}}",
            process.key.pid,
            start_kind,
            start_value,
            process.key.generation,
            json::named_bytes("comm", &process.comm),
            process.observed,
            process.valid_baseline,
            process.exited,
            process.samples
        )
        .map_err(|e| format!("write processes.jsonl: {e}"))?;
        if index < OVERVIEW_ROWS_PER_KIND {
            write_overview_process(overview, index, process, analysis.sample_records)?;
        }
    }
    finish(file, "processes.jsonl")?;
    Ok(row_count)
}

fn write_overview_process(
    overview: &mut Output,
    index: usize,
    process: &state::ProcessSummary,
    samples: u64,
) -> Result<(), String> {
    let (start_kind, start_value) = start_identity(&process.key.start);
    writeln!(
        overview,
        "{{\"schema\":{SCHEMA},\"kind\":\"process\",\"rank\":{},\"pid\":{},\
         \"start_kind\":\"{}\",\"start_value\":{},\"generation\":{},\
         {},\
         \"observed\":{},\"baseline_valid\":{},\"exited\":{},\"samples\":{},\
         \"sample_share_millionths\":{}}}",
        index.saturating_add(1),
        process.key.pid,
        start_kind,
        start_value,
        process.key.generation,
        overview_bytes("comm", &process.comm),
        process.observed,
        process.valid_baseline,
        process.exited,
        process.samples,
        sample_share_millionths(process.samples, samples),
    )
    .map_err(|e| format!("write overview process row: {e}"))
}

fn write_stacks(
    capture: &Path,
    analysis: &state::Analysis,
    symbolizer: &mut Symbolizer,
    budget: &OutputBudget,
    expansion: &mut ExpansionBudget,
) -> Result<StackRows, String> {
    symbolizer.begin_report();
    expansion.claim(
        analysis
            .stacks
            .len()
            .saturating_mul(std::mem::size_of::<(&state::StackKey, &u64)>()),
        "stack ordering",
    )?;
    let mut rows: Vec<_> = analysis.stacks.iter().collect();
    rows.sort_by(|(left_key, left_count), (right_key, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_key.cmp(right_key))
    });
    let mut structured = output(capture.join("stacks.jsonl"), budget)?;
    let mut folded = output(capture.join("stacks.folded"), budget)?;
    let mut hotspots: BTreeMap<HotspotKey, u64> = BTreeMap::new();
    let mut lines: BTreeMap<LineKey, u64> = BTreeMap::new();
    let mut unresolved_stacks = 0u64;
    let mut complete_stack_samples = 0u64;
    let mut truncated_stack_samples = 0u64;
    let mut unresolved_stack_samples = 0u64;
    let mut line_resolved_samples = 0u64;
    let mut line_unresolved_samples = 0u64;

    for (stack, count) in rows {
        let (start_kind, start_value) = start_identity(&stack.image.start);
        let mut transient_bytes = stack
            .frames
            .len()
            .saturating_mul(std::mem::size_of::<Option<Resolved>>());
        expansion.claim(transient_bytes, "resolved-frame roster")?;
        let mut resolved = Vec::with_capacity(stack.frames.len());
        for frame in &stack.frames {
            let max_resolved = expansion.remaining().saturating_sub(512) / 8;
            let value = symbolizer.resolve(frame, max_resolved);
            if let Some(value) = &value {
                let bytes = resolved_json_expansion(value);
                expansion.claim(bytes, "resolved frames")?;
                transient_bytes = transient_bytes.saturating_add(bytes);
            }
            resolved.push(value);
        }
        let reported_state = reported_stack_state(&stack.state, &resolved);
        match &reported_state {
            StackState::Complete => {
                complete_stack_samples = complete_stack_samples.saturating_add(*count);
            }
            StackState::Truncated(_) => {
                unresolved_stacks = unresolved_stacks.saturating_add(1);
                truncated_stack_samples = truncated_stack_samples.saturating_add(*count);
            }
            StackState::Unresolved(_) => {
                unresolved_stacks = unresolved_stacks.saturating_add(1);
                unresolved_stack_samples = unresolved_stack_samples.saturating_add(*count);
            }
        }
        write!(
            structured,
            "{{\"schema\":{SCHEMA},\"pid\":{},\"start_kind\":\"{}\",\"start_value\":{},\
             \"generation\":{},\"tid\":{},\
             \"state\":\"{}\",\"reason_bytes\":\"{}\",\"count\":{},\"frames\":[",
            stack.image.pid,
            start_kind,
            start_value,
            stack.image.generation,
            stack.tid,
            state_name(&reported_state),
            json::hex(state_reason(&reported_state)),
            count
        )
        .map_err(|e| format!("write stacks.jsonl: {e}"))?;
        for (number, (frame, symbol)) in stack.frames.iter().zip(&resolved).enumerate() {
            if number != 0 {
                structured.write_all(b",").map_err(|e| e.to_string())?;
            }
            write_frame(&mut structured, frame, symbol.as_ref())?;
        }
        structured
            .write_all(b"]}\n")
            .map_err(|e| format!("write stacks.jsonl: {e}"))?;

        let mut first_name = true;
        if !matches!(reported_state, StackState::Complete) {
            write!(folded, "[td:{}:", state_name(&reported_state)).map_err(|e| e.to_string())?;
            write_folded_escape(&mut folded, state_reason(&reported_state))?;
            folded.write_all(b"]").map_err(|e| e.to_string())?;
            first_name = false;
        }
        for (frame, symbol) in stack.frames.iter().zip(&resolved).rev() {
            if !first_name {
                folded.write_all(b";").map_err(|e| e.to_string())?;
            }
            let name = symbol
                .as_ref()
                .map(|value| value.function.as_slice())
                .unwrap_or_else(|| frame.path.as_slice());
            let fallback;
            let name = if name.is_empty() {
                fallback = format!("0x{:x}", frame.address);
                fallback.as_bytes()
            } else {
                name
            };
            write_folded_escape(&mut folded, name)?;
            first_name = false;
        }
        writeln!(folded, " {count}").map_err(|e| e.to_string())?;

        if let Some(frame) = stack.frames.first() {
            let symbol = resolved.first().and_then(Option::as_ref);
            let key = hotspot_key(&stack.image, frame, symbol);
            match hotspots.entry(key) {
                Entry::Occupied(mut entry) => {
                    let samples = entry.get().saturating_add(*count);
                    *entry.get_mut() = samples;
                }
                Entry::Vacant(entry) => {
                    expansion.claim(hotspot_heap_bytes(frame, symbol), "hotspot keys")?;
                    entry.insert(*count);
                }
            }
            let key = line_key(&stack.image, frame, symbol);
            if key.line_resolved {
                line_resolved_samples = line_resolved_samples.saturating_add(*count);
            } else {
                line_unresolved_samples = line_unresolved_samples.saturating_add(*count);
            }
            match lines.entry(key) {
                Entry::Occupied(mut entry) => {
                    let samples = entry.get().saturating_add(*count);
                    *entry.get_mut() = samples;
                }
                Entry::Vacant(entry) => {
                    expansion.claim(line_heap_bytes(frame, symbol), "source-line keys")?;
                    entry.insert(*count);
                }
            }
        }
        drop(resolved);
        drop(reported_state);
        expansion.release(transient_bytes);
    }
    finish(structured, "stacks.jsonl")?;
    finish(folded, "stacks.folded")?;
    Ok(StackRows {
        hotspots,
        lines,
        unresolved_stacks,
        complete_stack_samples,
        truncated_stack_samples,
        unresolved_stack_samples,
        line_resolved_samples,
        line_unresolved_samples,
    })
}

fn reported_stack_state(base: &StackState, resolved: &[Option<Resolved>]) -> StackState {
    if !matches!(base, StackState::Complete) {
        return base.clone();
    }
    if resolved.iter().any(Option::is_none) {
        return StackState::Unresolved(b"symbol-coverage-unavailable".to_vec());
    }
    if resolved
        .iter()
        .flatten()
        .any(|frame| frame.assembly_boundary)
    {
        return StackState::Truncated(b"assembly-coverage-boundary".to_vec());
    }
    StackState::Complete
}

fn write_frame(
    file: &mut Output,
    frame: &Frame,
    resolved: Option<&Resolved>,
) -> Result<(), String> {
    write!(
        file,
        "{{\"address\":{},\"object_offset\":{},\"device_major\":{},\"device_minor\":{},\
         \"inode\":{},\"inode_generation\":{},{}",
        frame.address,
        frame
            .relative
            .map_or_else(|| "null".into(), |value| value.to_string()),
        frame.major,
        frame.minor,
        frame.inode,
        frame.inode_generation,
        json::named_bytes("mapping_path", &frame.path)
    )
    .map_err(|e| e.to_string())?;
    if let Some(resolved) = resolved {
        write!(file, ",{}", resolved_json(resolved)).map_err(|e| e.to_string())?;
    } else {
        write!(
            file,
            ",\"line_resolved\":false,{},\"source_line\":null,\
             \"source_column\":null,\"discriminator\":null",
            json::named_bytes("source_file", &[])
        )
        .map_err(|e| e.to_string())?;
    }
    file.write_all(b"}").map_err(|e| e.to_string())
}

fn hotspot_key(image: &ImageKey, frame: &Frame, resolved: Option<&Resolved>) -> HotspotKey {
    match resolved {
        Some(resolved) => HotspotKey {
            image: image.clone(),
            resolved: true,
            function_address: resolved.function_address,
            function: resolved.function.clone(),
            object: resolved.object.clone(),
            build_id: resolved.build_id.clone(),
        },
        None => HotspotKey {
            image: image.clone(),
            resolved: false,
            function_address: frame.relative.unwrap_or(frame.address),
            function: Vec::new(),
            object: frame.path.clone(),
            build_id: Vec::new(),
        },
    }
}

fn line_key(image: &ImageKey, frame: &Frame, resolved: Option<&Resolved>) -> LineKey {
    match resolved {
        Some(resolved) => {
            let source = resolved.source.as_ref();
            LineKey {
                image: image.clone(),
                symbol_resolved: true,
                line_resolved: source.is_some(),
                function_address: resolved.function_address,
                object_address: source.map_or(resolved.object_address, |_| 0),
                function: resolved.function.clone(),
                object: resolved.object.clone(),
                build_id: resolved.build_id.clone(),
                source_file: source.map_or_else(Vec::new, |value| value.file.clone()),
                source_line: source.map_or(0, |value| value.line),
                source_column: source.map_or(0, |value| value.column),
                discriminator: source.map_or(0, |value| value.discriminator),
            }
        }
        None => LineKey {
            image: image.clone(),
            symbol_resolved: false,
            line_resolved: false,
            function_address: 0,
            object_address: frame.relative.unwrap_or(frame.address),
            function: Vec::new(),
            object: frame.path.clone(),
            build_id: Vec::new(),
            source_file: Vec::new(),
            source_line: 0,
            source_column: 0,
            discriminator: 0,
        },
    }
}

fn write_hotspots(
    capture: &Path,
    hotspots: BTreeMap<HotspotKey, u64>,
    budget: &OutputBudget,
    overview: &mut Output,
    analysis: &state::Analysis,
) -> Result<usize, String> {
    let mut rows: Vec<_> = hotspots.into_iter().collect();
    rows.sort_by(|(left_key, left_count), (right_key, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_key.cmp(right_key))
    });
    let row_count = rows.len();
    let mut file = output(capture.join("hotspots.jsonl"), budget)?;
    for (index, (hotspot, count)) in rows.into_iter().enumerate() {
        let (start_kind, start_value) = start_identity(&hotspot.image.start);
        writeln!(
            file,
            "{{\"schema\":{SCHEMA},\"pid\":{},\"start_kind\":\"{}\",\"start_value\":{},\
             \"generation\":{},\
             \"resolved\":{},{},{},\"build_id\":\"{}\",\"function_address\":{},\"samples\":{}}}",
            hotspot.image.pid,
            start_kind,
            start_value,
            hotspot.image.generation,
            hotspot.resolved,
            json::named_bytes("object", &hotspot.object),
            json::named_bytes("function", &hotspot.function),
            json::hex(&hotspot.build_id),
            hotspot.function_address,
            count
        )
        .map_err(|e| format!("write hotspots.jsonl: {e}"))?;
        if index < OVERVIEW_ROWS_PER_KIND {
            write_overview_hotspot(overview, index, &hotspot, count, analysis.sample_records)?;
        }
    }
    finish(file, "hotspots.jsonl")?;
    Ok(row_count)
}

fn write_overview_hotspot(
    overview: &mut Output,
    index: usize,
    hotspot: &HotspotKey,
    count: u64,
    samples: u64,
) -> Result<(), String> {
    let (start_kind, start_value) = start_identity(&hotspot.image.start);
    writeln!(
        overview,
        "{{\"schema\":{SCHEMA},\"kind\":\"hotspot\",\"rank\":{},\"pid\":{},\
         \"start_kind\":\"{}\",\"start_value\":{},\"generation\":{},\
         \"resolved\":{},{},{},\"build_id\":\"{}\",\"function_address\":{},\
         \"samples\":{},\"sample_share_millionths\":{}}}",
        index.saturating_add(1),
        hotspot.image.pid,
        start_kind,
        start_value,
        hotspot.image.generation,
        hotspot.resolved,
        overview_bytes("object", &hotspot.object),
        overview_bytes("function", &hotspot.function),
        json::hex(&hotspot.build_id),
        hotspot.function_address,
        count,
        sample_share_millionths(count, samples),
    )
    .map_err(|e| format!("write overview hotspot row: {e}"))
}

fn write_lines(
    capture: &Path,
    lines: BTreeMap<LineKey, u64>,
    budget: &OutputBudget,
    overview: &mut Output,
    analysis: &state::Analysis,
) -> Result<usize, String> {
    let mut rows: Vec<_> = lines.into_iter().collect();
    rows.sort_by(|(left_key, left_count), (right_key, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_key.cmp(right_key))
    });
    let row_count = rows.len();
    let mut file = output(capture.join("lines.jsonl"), budget)?;
    for (index, (line, count)) in rows.into_iter().enumerate() {
        let (start_kind, start_value) = start_identity(&line.image.start);
        let function_address = line
            .symbol_resolved
            .then_some(line.function_address)
            .map_or_else(|| "null".into(), |value| value.to_string());
        let object_address = (!line.line_resolved)
            .then_some(line.object_address)
            .map_or_else(|| "null".into(), |value| value.to_string());
        let source_line = line
            .line_resolved
            .then_some(line.source_line)
            .map_or_else(|| "null".into(), |value| value.to_string());
        let source_column = line
            .line_resolved
            .then_some(line.source_column)
            .map_or_else(|| "null".into(), |value| value.to_string());
        let discriminator = line
            .line_resolved
            .then_some(line.discriminator)
            .map_or_else(|| "null".into(), |value| value.to_string());
        writeln!(
            file,
            "{{\"schema\":{SCHEMA},\"pid\":{},\"start_kind\":\"{}\",\"start_value\":{},\
             \"generation\":{},\"symbol_resolved\":{},\"line_resolved\":{},{},{},\
             \"build_id\":\"{}\",\"function_address\":{},\"object_address\":{},{},\
             \"source_line\":{},\"source_column\":{},\"discriminator\":{},\"samples\":{}}}",
            line.image.pid,
            start_kind,
            start_value,
            line.image.generation,
            line.symbol_resolved,
            line.line_resolved,
            json::named_bytes("object", &line.object),
            json::named_bytes("function", &line.function),
            json::hex(&line.build_id),
            function_address,
            object_address,
            json::named_bytes("source_file", &line.source_file),
            source_line,
            source_column,
            discriminator,
            count
        )
        .map_err(|e| format!("write lines.jsonl: {e}"))?;
        if index < OVERVIEW_ROWS_PER_KIND {
            write_overview_line(overview, index, &line, count, analysis.sample_records)?;
        }
    }
    finish(file, "lines.jsonl")?;
    Ok(row_count)
}

fn write_overview_line(
    overview: &mut Output,
    index: usize,
    line: &LineKey,
    count: u64,
    samples: u64,
) -> Result<(), String> {
    let (start_kind, start_value) = start_identity(&line.image.start);
    let function_address = line
        .symbol_resolved
        .then_some(line.function_address)
        .map_or_else(|| "null".into(), |value| value.to_string());
    let object_address = (!line.line_resolved)
        .then_some(line.object_address)
        .map_or_else(|| "null".into(), |value| value.to_string());
    let source_line = line
        .line_resolved
        .then_some(line.source_line)
        .map_or_else(|| "null".into(), |value| value.to_string());
    let source_column = line
        .line_resolved
        .then_some(line.source_column)
        .map_or_else(|| "null".into(), |value| value.to_string());
    let discriminator = line
        .line_resolved
        .then_some(line.discriminator)
        .map_or_else(|| "null".into(), |value| value.to_string());
    writeln!(
        overview,
        "{{\"schema\":{SCHEMA},\"kind\":\"line\",\"rank\":{},\"pid\":{},\
         \"start_kind\":\"{}\",\"start_value\":{},\"generation\":{},\
         \"symbol_resolved\":{},\"line_resolved\":{},{},{},\"build_id\":\"{}\",\
         \"function_address\":{},\"object_address\":{},{},\"source_line\":{},\
         \"source_column\":{},\"discriminator\":{},\"samples\":{},\
         \"sample_share_millionths\":{}}}",
        index.saturating_add(1),
        line.image.pid,
        start_kind,
        start_value,
        line.image.generation,
        line.symbol_resolved,
        line.line_resolved,
        overview_bytes("object", &line.object),
        overview_bytes("function", &line.function),
        json::hex(&line.build_id),
        function_address,
        object_address,
        overview_bytes("source_file", &line.source_file),
        source_line,
        source_column,
        discriminator,
        count,
        sample_share_millionths(count, samples),
    )
    .map_err(|e| format!("write overview line row: {e}"))
}

#[allow(clippy::too_many_arguments)]
fn write_overview_summary(
    overview: &mut Output,
    analysis: &state::Analysis,
    symbolizer: &Symbolizer,
    process_rows: usize,
    hotspot_rows: usize,
    line_rows: usize,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
) -> Result<(), String> {
    let omitted_errors = analysis
        .omitted_errors
        .saturating_add(symbolizer.omitted_errors());
    writeln!(
        overview,
        "{{\"schema\":{SCHEMA},\"kind\":\"capture\",\"samples\":{},\
         \"process_rows\":{},\"hotspot_rows\":{},\"line_rows\":{},\
         \"rows_per_kind_limit\":{OVERVIEW_ROWS_PER_KIND},\
         \"complete_stack_samples\":{},\"truncated_stack_samples\":{},\
         \"unresolved_stack_samples\":{},\"line_resolved_samples\":{},\
         \"line_unresolved_samples\":{},\"lost\":{},\"corrupt\":{},\
         \"omitted_errors\":{}}}",
        analysis.sample_records,
        process_rows,
        hotspot_rows,
        line_rows,
        complete_stack_samples,
        truncated_stack_samples,
        unresolved_stack_samples,
        line_resolved_samples,
        line_unresolved_samples,
        analysis.lost_records,
        analysis.corrupt_records,
        omitted_errors,
    )
    .map_err(|e| format!("write overview capture row: {e}"))
}

pub(crate) fn sample_share_millionths(samples: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let share = u128::from(samples).saturating_mul(1_000_000) / u128::from(total);
    u64::try_from(share).unwrap_or(u64::MAX)
}

struct OverviewBytes<'a> {
    name: &'a str,
    bytes: &'a [u8],
}

fn overview_bytes<'a>(name: &'a str, bytes: &'a [u8]) -> OverviewBytes<'a> {
    OverviewBytes { name, bytes }
}

impl std::fmt::Display for OverviewBytes<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let length = self.bytes.len();
        let prefix = self
            .bytes
            .get(..length.min(OVERVIEW_FIELD_PREFIX_BYTES))
            .unwrap_or_default();
        let truncated = prefix.len() != length;
        if !truncated {
            if let Ok(text) = std::str::from_utf8(prefix) {
                write!(out, "\"{}\":\"", self.name)?;
                for character in text.chars() {
                    match character {
                        '"' => out.write_str("\\\"")?,
                        '\\' => out.write_str("\\\\")?,
                        '\n' => out.write_str("\\n")?,
                        '\r' => out.write_str("\\r")?,
                        '\t' => out.write_str("\\t")?,
                        c if c <= '\u{1f}' => write!(out, "\\u{:04x}", c as u32)?,
                        c => out.write_char(c)?,
                    }
                }
                out.write_str("\",")?;
            }
        }
        write!(out, "\"{}_bytes_prefix\":\"", self.name)?;
        for byte in prefix {
            write!(out, "{byte:02x}")?;
        }
        write!(
            out,
            "\",\"{}_bytes_length\":{},\"{}_truncated\":{}",
            self.name, length, self.name, truncated
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn write_manifest(
    capture: &Path,
    meta: &Meta,
    analysis: &state::Analysis,
    unknown_records: u64,
    unresolved_stacks: u64,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
    symbolizer: &Symbolizer,
    budget: &OutputBudget,
    expansion: &mut ExpansionBudget,
) -> Result<(), String> {
    let prefix = live_manifest_prefix(meta, Some(analysis.sample_records))?;
    write_manifest_tail(
        capture,
        &prefix,
        analysis,
        unknown_records,
        unresolved_stacks,
        complete_stack_samples,
        truncated_stack_samples,
        unresolved_stack_samples,
        line_resolved_samples,
        line_unresolved_samples,
        symbolizer,
        budget,
        expansion,
        false,
        "manifest.json.next",
    )?;
    fs::rename(
        capture.join("manifest.json.next"),
        capture.join("manifest.json"),
    )
    .map_err(|e| format!("publish manifest.json: {e}"))?;
    sync_dir(capture)
}

fn live_manifest_prefix(meta: &Meta, samples: Option<u64>) -> Result<String, String> {
    let mut cpu_json = String::new();
    let mut coverage_json = String::new();
    for (number, cpu) in meta.cpus.iter().enumerate() {
        if number != 0 {
            cpu_json.push(',');
        }
        let _ = write!(cpu_json, "{cpu}");
    }
    for (number, (cpu, start_ns, end_ns)) in meta.coverage.iter().enumerate() {
        if number != 0 {
            coverage_json.push(',');
        }
        let _ = write!(
            coverage_json,
            "{{\"cpu\":{cpu},\"start_ns\":{},\"end_ns\":{}}}",
            start_ns, end_ns
        );
    }
    if cpu_json.len().saturating_add(coverage_json.len()) > MAX_REPORT_EXPANSION_BYTES {
        return Err("CPU manifest fields exceed the report expansion budget".into());
    }
    let coverage_duration = coverage_duration_ns(meta)?;
    let effective_rate = samples
        .and_then(|samples| effective_rate_millihz(meta, samples))
        .map_or_else(|| "null".to_string(), |rate| rate.to_string());
    Ok(format!(
        "{{\"schema\":{SCHEMA},\"profiler_build\":{},\"deployment\":{},\"boot_id\":{},\
         \"monotonic_start_ns\":{},\"monotonic_end_ns\":{},\"wall_start_seconds\":{},\
         \"rate_mode\":\"per-cpu-frequency-target\",\"configured_rate_hz\":{},\
         \"coverage_duration_ns\":{},\"effective_rate_millihz\":{},\
         \"cpus\":[{}],\"coverage\":[{}]",
        json::string(meta.profiler_build.as_bytes()).unwrap_or_else(|| "null".into()),
        json::string(meta.deployment.as_bytes()).unwrap_or_else(|| "null".into()),
        json::string(meta.boot_id.as_bytes()).unwrap_or_else(|| "null".into()),
        meta.start_ns,
        meta.end_ns,
        meta.wall_start_seconds,
        meta.rate_hz,
        coverage_duration,
        effective_rate,
        cpu_json,
        coverage_json,
    ))
}

#[allow(clippy::too_many_arguments)]
fn write_manifest_tail(
    capture: &Path,
    prefix: &str,
    analysis: &state::Analysis,
    unknown_records: u64,
    unresolved_stacks: u64,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
    symbolizer: &Symbolizer,
    budget: &OutputBudget,
    expansion: &mut ExpansionBudget,
    regenerated: bool,
    manifest_name: &str,
) -> Result<(), String> {
    expansion.claim(prefix.len(), "manifest prefix")?;
    let mut error_json = String::new();
    for error in &analysis.errors {
        let worst_case = error
            .message
            .len()
            .checked_mul(8)
            .and_then(|value| value.checked_add(192))
            .ok_or("error manifest expansion overflow")?;
        expansion.claim(worst_case, "error manifest fields")?;
        if !error_json.is_empty() {
            error_json.push(',');
        }
        let _ = write!(
            error_json,
            "{{\"cpu\":{},\"start_ns\":{},\"end_ns\":{},\"count\":{},{}}}",
            error.cpu,
            error.start_ns,
            error.end_ns,
            error.count,
            json::named_bytes("message", &error.message)
        );
    }
    for error in &symbolizer.errors {
        let worst_case = error
            .len()
            .checked_mul(8)
            .and_then(|value| value.checked_add(192))
            .ok_or("error manifest expansion overflow")?;
        expansion.claim(worst_case, "error manifest fields")?;
        if !error_json.is_empty() {
            error_json.push(',');
        }
        let _ = write!(
            error_json,
            "{{\"cpu\":null,\"start_ns\":null,\"end_ns\":null,\"count\":1,{}}}",
            json::named_bytes("message", error.as_bytes())
        );
    }
    let omitted_errors = analysis
        .omitted_errors
        .saturating_add(symbolizer.omitted_errors());
    let identities = symbolizer.identities_json(expansion.remaining() / 2)?;
    expansion.claim(
        identities.len().saturating_mul(2),
        "object identity manifest fields",
    )?;
    let mut file = output(capture.join(manifest_name), budget)?;
    let regenerated_json = if regenerated {
        ",\"regenerated\":true,\"raw_capture\":\"../samples.bin\""
    } else {
        ""
    };
    writeln!(
        file,
        "{}{regenerated_json},\"samples\":{},\"tasks\":{},\"mappings\":{},\"context_switches\":{},\
         \"lost\":{},\"corrupt\":{},\"ignored_perf_records\":{},\"unknown_raw_records\":{},\
         \"omitted_errors\":{},\"unresolved_stacks\":{},\
         \"complete_stack_samples\":{},\"truncated_stack_samples\":{},\
         \"unresolved_stack_samples\":{},\
         \"line_resolved_samples\":{},\"line_unresolved_samples\":{},\
         \"objects\":[{}],\"errors\":[{}]}}",
        prefix,
        analysis.sample_records,
        analysis.task_records,
        analysis.mapping_records,
        analysis.switch_records,
        analysis.lost_records,
        analysis.corrupt_records,
        analysis.ignored_records,
        unknown_records,
        omitted_errors,
        unresolved_stacks,
        complete_stack_samples,
        truncated_stack_samples,
        unresolved_stack_samples,
        line_resolved_samples,
        line_unresolved_samples,
        identities,
        error_json
    )
    .map_err(|e| format!("write manifest.json: {e}"))?;
    finish(file, "manifest.json")
}

fn effective_rate_millihz(meta: &Meta, samples: u64) -> Option<u64> {
    effective_rate_for_duration(samples, coverage_duration_ns(meta).ok()?)
}

fn coverage_duration_ns(meta: &Meta) -> Result<u128, String> {
    meta.coverage
        .iter()
        .try_fold(0u128, |total, (_, start, end)| {
            total.checked_add(u128::from(end.saturating_sub(*start)))
        })
        .ok_or("coverage duration overflow".into())
}

fn effective_rate_for_duration(samples: u64, coverage_ns: u128) -> Option<u64> {
    if coverage_ns == 0 {
        return None;
    }
    let scaled = u128::from(samples).checked_mul(1_000_000_000_000)?;
    u64::try_from(scaled / coverage_ns).ok()
}

fn state_name(state: &StackState) -> &'static str {
    match state {
        StackState::Complete => "complete",
        StackState::Truncated(_) => "truncated",
        StackState::Unresolved(_) => "unresolved",
    }
}

fn start_identity(start: &StartIdentity) -> (&'static str, u64) {
    match start {
        StartIdentity::Unknown => ("unknown", 0),
        StartIdentity::ProcTicks(value) => ("proc-start-ticks", *value),
        StartIdentity::PerfTimeNs(value) => ("perf-fork-time-ns", *value),
    }
}

fn state_reason(state: &StackState) -> &[u8] {
    match state {
        StackState::Complete => b"",
        StackState::Truncated(reason) | StackState::Unresolved(reason) => reason,
    }
}

fn write_folded_escape(file: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let bytes = if bytes.starts_with(b"[td:") {
        file.write_all(b"\\x5b").map_err(|e| e.to_string())?;
        bytes.get(1..).unwrap_or_default()
    } else {
        bytes
    };
    for byte in bytes {
        match byte {
            b';' => file.write_all(b"\\x3b"),
            b' ' => file.write_all(b"\\x20"),
            b'\\' => file.write_all(b"\\x5c"),
            0x21..=0x7e => file.write_all(&[*byte]),
            _ => {
                write!(file, "\\x{byte:02x}")
            }
        }
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
fn folded_escape(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    let _ = write_folded_escape(&mut out, bytes);
    String::from_utf8(out).unwrap_or_default()
}

fn resolved_json_expansion(resolved: &Resolved) -> usize {
    [
        resolved.function.len(),
        resolved.object.len(),
        resolved.debug.len(),
        resolved.build_id.len(),
        resolved.provenance.len(),
        resolved.source.as_ref().map_or(0, |value| value.file.len()),
    ]
    .into_iter()
    .fold(std::mem::size_of::<Resolved>(), usize::saturating_add)
    .saturating_mul(8)
    .saturating_add(512)
}

fn hotspot_heap_bytes(frame: &Frame, resolved: Option<&Resolved>) -> usize {
    let fields = match resolved {
        Some(resolved) => resolved
            .function
            .len()
            .saturating_add(resolved.object.len())
            .saturating_add(resolved.build_id.len()),
        None => frame.path.len().saturating_add(32),
    };
    std::mem::size_of::<HotspotKey>()
        .saturating_add(fields)
        .saturating_add(64)
        .saturating_mul(2)
}

fn line_heap_bytes(frame: &Frame, resolved: Option<&Resolved>) -> usize {
    let fields = match resolved {
        Some(resolved) => resolved
            .function
            .len()
            .saturating_add(resolved.object.len())
            .saturating_add(resolved.build_id.len())
            .saturating_add(resolved.source.as_ref().map_or(0, |value| value.file.len())),
        None => frame.path.len().saturating_add(32),
    };
    std::mem::size_of::<LineKey>()
        .saturating_add(fields)
        .saturating_add(96)
        .saturating_mul(2)
}

struct ExpansionBudget {
    remaining: usize,
}

impl ExpansionBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_REPORT_EXPANSION_BYTES,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    fn claim(&mut self, bytes: usize, label: &str) -> Result<(), String> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| format!("{label} expand beyond {MAX_REPORT_EXPANSION_BYTES} bytes"))?;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.remaining = self
            .remaining
            .saturating_add(bytes)
            .min(MAX_REPORT_EXPANSION_BYTES);
    }
}

#[derive(Clone)]
struct OutputBudget {
    remaining: Rc<Cell<u64>>,
    limit: u64,
}

impl OutputBudget {
    fn new() -> Self {
        Self::with_limit(MAX_REPORT_BYTES)
    }

    fn with_limit(limit: u64) -> Self {
        Self {
            remaining: Rc::new(Cell::new(limit)),
            limit,
        }
    }

    fn claim(&self, bytes: u64, label: &str) -> Result<(), String> {
        let remaining = self.remaining.get();
        self.remaining.set(
            remaining
                .checked_sub(bytes)
                .ok_or_else(|| format!("{label} exceeds combined report limit {}", self.limit))?,
        );
        Ok(())
    }
}

struct Output {
    file: BufWriter<File>,
    budget: OutputBudget,
    file_budget: Option<OutputBudget>,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("report write length overflow"))?;
        let remaining = self.budget.remaining.get();
        if length > remaining {
            return Err(io::Error::other(format!(
                "combined report output exceeds {} bytes",
                self.budget.limit
            )));
        }
        let file_remaining = self
            .file_budget
            .as_ref()
            .map(|budget| budget.remaining.get());
        if let (Some(budget), Some(file_remaining)) = (&self.file_budget, file_remaining) {
            if length > file_remaining {
                return Err(io::Error::other(format!(
                    "report file output exceeds {} bytes",
                    budget.limit
                )));
            }
        }
        let written = self.file.write(bytes)?;
        self.budget
            .remaining
            .set(remaining.saturating_sub(written as u64));
        if let (Some(budget), Some(file_remaining)) = (&self.file_budget, file_remaining) {
            budget
                .remaining
                .set(file_remaining.saturating_sub(written as u64));
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn output(path: PathBuf, budget: &OutputBudget) -> Result<Output, String> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o640))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(Output {
        file: BufWriter::new(file),
        budget: budget.clone(),
        file_budget: None,
    })
}

fn output_bounded(path: PathBuf, budget: &OutputBudget, limit: u64) -> Result<Output, String> {
    let mut file = output(path, budget)?;
    file.file_budget = Some(OutputBudget::with_limit(limit));
    Ok(file)
}

fn finish(mut file: Output, label: &str) -> Result<(), String> {
    file.flush().map_err(|e| format!("flush {label}: {e}"))?;
    file.file
        .get_ref()
        .sync_all()
        .map_err(|e| format!("fsync {label}: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        effective_rate_millihz, finish, folded_escape, generate, hotspot_key, line_key, output,
        output_bounded, overview_bytes, regenerate, reported_stack_state,
        validate_identity_metadata, write_lines, write_overview_hotspot, write_overview_line,
        write_overview_process, write_overview_summary, write_pending_manifest, ExpansionBudget,
        HotspotKey, LineKey, Meta, OutputBudget, RegenerationLock, MAX_CAPTURE_METADATA_BYTES,
        MAX_FAILURE_MARKER_BYTES, MAX_PROFILE_CPUS, MAX_REPORT_BYTES, MAX_REPORT_EXPANSION_BYTES,
    };
    use crate::event::{Event, Kind, StartIdentity};
    use crate::state::{Analysis, Frame, ImageKey, ProcessSummary, StackKey, StackState};
    use crate::symbol::{Resolved, Symbolizer};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn folded_symbols_cannot_enter_the_reserved_namespace() {
        assert_eq!(folded_escape(b"[td:fake]"), "\\x5btd:fake]");
        assert_eq!(folded_escape(b"a;b c\\d"), "a\\x3bb\\x20c\\x5cd");
    }

    #[test]
    fn report_state_exposes_symbol_and_assembly_coverage_boundaries() {
        let resolved = Resolved {
            function: b"f".to_vec(),
            object: b"o".to_vec(),
            debug: b"d".to_vec(),
            build_id: vec![0; 20],
            provenance: b"store-item=x".to_vec(),
            object_address: 1,
            function_address: 1,
            assembly_boundary: false,
            source: None,
        };
        assert_eq!(
            reported_stack_state(&StackState::Complete, &[Some(resolved.clone())]),
            StackState::Complete
        );
        assert!(matches!(
            reported_stack_state(&StackState::Complete, &[None]),
            StackState::Unresolved(_)
        ));
        let mut boundary = resolved;
        boundary.assembly_boundary = true;
        assert!(matches!(
            reported_stack_state(&StackState::Complete, &[Some(boundary)]),
            StackState::Truncated(_)
        ));
    }

    #[test]
    fn resolved_hotspots_aggregate_by_function_not_instruction() {
        let image = ImageKey {
            pid: 7,
            start: StartIdentity::ProcTicks(1),
            generation: 0,
        };
        let frame = |address| Frame {
            address,
            relative: Some(address),
            major: 1,
            minor: 2,
            inode: 3,
            inode_generation: 0,
            path: b"mapped".to_vec(),
        };
        let resolved = |object_address| Resolved {
            function: b"function".to_vec(),
            object: b"object".to_vec(),
            debug: b"debug".to_vec(),
            build_id: vec![1; 20],
            provenance: b"store-item=x".to_vec(),
            object_address,
            function_address: 0x100,
            assembly_boundary: false,
            source: None,
        };
        assert_eq!(
            hotspot_key(&image, &frame(0x111), Some(&resolved(0x111))),
            hotspot_key(&image, &frame(0x122), Some(&resolved(0x122)))
        );
        let unresolved = hotspot_key(&image, &frame(0x133), None);
        assert!(!unresolved.resolved);
        assert!(unresolved.function.is_empty());
        assert_eq!(unresolved.function_address, 0x133);

        let located = |object_address, line| Resolved {
            source: Some(crate::dwarf::Location {
                file: b"/td-build/src/main.rs".to_vec(),
                line,
                column: 7,
                discriminator: 0,
            }),
            ..resolved(object_address)
        };
        assert_eq!(
            line_key(&image, &frame(0x111), Some(&located(0x111, 9))),
            line_key(&image, &frame(0x122), Some(&located(0x122, 9)))
        );
        assert_ne!(
            line_key(&image, &frame(0x111), Some(&located(0x111, 9))),
            line_key(&image, &frame(0x111), Some(&located(0x111, 10)))
        );
        let unresolved_line = line_key(&image, &frame(0x133), Some(&resolved(0x133)));
        assert!(unresolved_line.symbol_resolved);
        assert!(!unresolved_line.line_resolved);
        assert_eq!(unresolved_line.object_address, 0x133);
    }

    #[test]
    fn resolved_line_rows_serialize_every_location_identity() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-resolved-line-row-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let image = ImageKey {
            pid: 7,
            start: StartIdentity::ProcTicks(11),
            generation: 2,
        };
        let frame = Frame {
            address: 0x101,
            relative: Some(0x101),
            major: 1,
            minor: 2,
            inode: 3,
            inode_generation: 0,
            path: b"mapped".to_vec(),
        };
        let resolved = Resolved {
            function: b"work".to_vec(),
            object: b"/td/store/object/bin/work".to_vec(),
            debug: b"/td/store/object/lib/debug/bin/work.debug".to_vec(),
            build_id: vec![0xab; 20],
            provenance: b"store-item=object".to_vec(),
            object_address: 0x101,
            function_address: 0x100,
            assembly_boundary: false,
            source: Some(crate::dwarf::Location {
                file: b"src/main.rs".to_vec(),
                line: 42,
                column: 7,
                discriminator: 3,
            }),
        };
        let mut rows = std::collections::BTreeMap::new();
        rows.insert(line_key(&image, &frame, Some(&resolved)), 9);
        let budget = OutputBudget::new();
        let mut overview = output(root.join("overview.jsonl"), &budget).unwrap();
        let analysis = Analysis {
            sample_records: 9,
            ..Analysis::default()
        };
        write_lines(&root, rows, &budget, &mut overview, &analysis).unwrap();
        finish(overview, "overview.jsonl").unwrap();
        let row = std::fs::read_to_string(root.join("lines.jsonl")).unwrap();
        assert!(row.contains("\"symbol_resolved\":true,\"line_resolved\":true"));
        assert!(row.contains("\"function_address\":256,\"object_address\":null"));
        assert!(row.contains("\"source_file\":\"src/main.rs\""));
        assert!(row
            .contains("\"source_line\":42,\"source_column\":7,\"discriminator\":3,\"samples\":9"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overview_is_a_bounded_ranked_entry_point_with_weighted_quality() {
        let root =
            std::env::temp_dir().join(format!("td-profiler-overview-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let mut analysis = Analysis::default();
        for pid in 1..=40u32 {
            let samples = u64::from(pid);
            let image = ImageKey {
                pid,
                start: StartIdentity::ProcTicks(u64::from(pid)),
                generation: 0,
            };
            analysis.processes.insert(
                image.clone(),
                ProcessSummary {
                    key: image.clone(),
                    comm: format!("process-{pid}").into_bytes(),
                    observed: true,
                    valid_baseline: true,
                    exited: false,
                    samples,
                },
            );
            analysis.stacks.insert(
                StackKey {
                    image,
                    tid: pid,
                    state: StackState::Complete,
                    frames: vec![Frame {
                        address: 0x1000 + u64::from(pid),
                        relative: Some(0x1000 + u64::from(pid)),
                        major: 1,
                        minor: 2,
                        inode: u64::from(pid),
                        inode_generation: 0,
                        path: format!("/mapped/process-{pid}").into_bytes(),
                    }],
                },
                samples,
            );
            analysis.sample_records = analysis.sample_records.saturating_add(samples);
        }
        let meta = Meta {
            profiler_build: "profiler".into(),
            deployment: "deployment".into(),
            boot_id: "boot".into(),
            start_ns: 1,
            end_ns: 2,
            wall_start_seconds: 3,
            rate_hz: 99,
            cpus: vec![0],
            coverage: vec![(0, 1, 2)],
        };
        let mut symbolizer = Symbolizer::from_index(None);
        generate(&root, &meta, &mut symbolizer, &analysis, 0).unwrap();

        let overview = std::fs::read_to_string(root.join("overview.jsonl")).unwrap();
        assert_eq!(overview.lines().count(), 97);
        assert_eq!(overview.matches("\"kind\":\"process\"").count(), 32);
        assert_eq!(overview.matches("\"kind\":\"hotspot\"").count(), 32);
        assert_eq!(overview.matches("\"kind\":\"line\"").count(), 32);
        assert_eq!(overview.matches("\"kind\":\"capture\"").count(), 1);
        assert!(overview.contains("\"kind\":\"process\",\"rank\":1,\"pid\":40"));
        assert!(!overview.contains("\"rank\":33"));
        assert!(overview.contains("\"samples\":820,\"process_rows\":40"));
        assert!(overview.contains("\"unresolved_stack_samples\":820"));
        assert!(overview.contains("\"line_unresolved_samples\":820"));
        assert!(overview
            .lines()
            .last()
            .unwrap()
            .contains("\"kind\":\"capture\""));
        let manifest = std::fs::read_to_string(root.join("manifest.json")).unwrap();
        assert!(manifest.contains("\"complete_stack_samples\":0"));
        assert!(manifest.contains("\"truncated_stack_samples\":0"));
        assert!(manifest.contains("\"unresolved_stack_samples\":820"));
        crate::evidence::validate_overview(&root, &manifest, 820, 0, 0, 820, 0, 820).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overview_fields_and_file_have_independent_hard_bounds() {
        let long = vec![b'a'; super::OVERVIEW_FIELD_PREFIX_BYTES + 1];
        let rendered = format!("{}", overview_bytes("field", &long));
        assert!(!rendered.starts_with("\"field\":"));
        assert!(rendered.contains("\"field_bytes_length\":1025"));
        assert!(rendered.contains("\"field_truncated\":true"));
        assert_eq!(
            rendered
                .split("\"field_bytes_prefix\":\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .len(),
            super::OVERVIEW_FIELD_PREFIX_BYTES * 2
        );
        let invalid = format!("{}", overview_bytes("field", &[0xff]));
        assert!(!invalid.starts_with("\"field\":"));
        assert!(invalid.contains("\"field_bytes_prefix\":\"ff\""));

        let path = std::env::temp_dir().join(format!(
            "td-profiler-overview-output-bound-test-{}",
            std::process::id()
        ));
        let budget = OutputBudget::new();
        let mut file = output_bounded(path.clone(), &budget, 3).unwrap();
        file.write_all(b"123").unwrap();
        assert!(file
            .write_all(b"4")
            .unwrap_err()
            .to_string()
            .contains("report file output exceeds 3 bytes"));
        finish(file, "bounded overview").unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn maximum_expansion_of_the_fixed_overview_roster_fits_its_file_cap() {
        let path = std::env::temp_dir().join(format!(
            "td-profiler-overview-maximum-test-{}",
            std::process::id()
        ));
        let budget = OutputBudget::new();
        let mut overview =
            output_bounded(path.clone(), &budget, super::MAX_OVERVIEW_BYTES).unwrap();
        let bytes = vec![0x1f; super::OVERVIEW_FIELD_PREFIX_BYTES];
        let image = ImageKey {
            pid: u32::MAX,
            start: StartIdentity::ProcTicks(u64::MAX),
            generation: u64::MAX,
        };
        let process = ProcessSummary {
            key: image.clone(),
            comm: bytes.clone(),
            observed: true,
            valid_baseline: true,
            exited: true,
            samples: u64::MAX,
        };
        let hotspot = HotspotKey {
            image: image.clone(),
            resolved: true,
            function_address: u64::MAX,
            function: bytes.clone(),
            object: bytes.clone(),
            build_id: vec![u8::MAX; 20],
        };
        let line = LineKey {
            image,
            symbol_resolved: true,
            line_resolved: true,
            function_address: u64::MAX,
            object_address: u64::MAX,
            function: bytes.clone(),
            object: bytes.clone(),
            build_id: vec![u8::MAX; 20],
            source_file: bytes,
            source_line: u64::MAX,
            source_column: u64::MAX,
            discriminator: u64::MAX,
        };
        for index in 0..super::OVERVIEW_ROWS_PER_KIND {
            write_overview_process(&mut overview, index, &process, u64::MAX).unwrap();
            write_overview_hotspot(&mut overview, index, &hotspot, u64::MAX, u64::MAX).unwrap();
            write_overview_line(&mut overview, index, &line, u64::MAX, u64::MAX).unwrap();
        }
        let analysis = Analysis {
            sample_records: u64::MAX,
            lost_records: u64::MAX,
            corrupt_records: u64::MAX,
            omitted_errors: u64::MAX,
            ..Analysis::default()
        };
        write_overview_summary(
            &mut overview,
            &analysis,
            &Symbolizer::from_index(None),
            usize::MAX,
            usize::MAX,
            usize::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
        )
        .unwrap();
        finish(overview, "maximum overview").unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 1024 * 1024);
        assert!(size <= super::MAX_OVERVIEW_BYTES);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn live_report_and_failure_metadata_share_one_fixed_allowance() {
        assert_eq!(MAX_REPORT_BYTES, 128 * 1024 * 1024);
        assert_eq!(super::MAX_OVERVIEW_BYTES, 2 * 1024 * 1024);
        assert_eq!(MAX_REPORT_EXPANSION_BYTES, 128 * 1024 * 1024);
        let derived = MAX_REPORT_BYTES - MAX_CAPTURE_METADATA_BYTES;
        assert_eq!(derived + MAX_CAPTURE_METADATA_BYTES, MAX_REPORT_BYTES);
        assert!(super::MAX_OVERVIEW_BYTES < derived);
        const { assert!(MAX_FAILURE_MARKER_BYTES < MAX_CAPTURE_METADATA_BYTES) };
        validate_identity_metadata("/td/store/profiler", "deployment").unwrap();
        assert!(validate_identity_metadata(&"\u{1}".repeat(200_000), "deployment").is_err());

        let cpus: Vec<u32> = (0..MAX_PROFILE_CPUS as u32).collect();
        let meta = Meta {
            profiler_build: "profiler".into(),
            deployment: "deployment".into(),
            boot_id: "00000000-0000-0000-0000-000000000000".into(),
            start_ns: u64::MAX,
            end_ns: u64::MAX,
            wall_start_seconds: u64::MAX,
            rate_hz: 10_000,
            coverage: cpus
                .iter()
                .copied()
                .map(|cpu| (cpu, u64::MAX, u64::MAX))
                .collect(),
            cpus,
        };
        let prefix = super::live_manifest_prefix(&meta, Some(u64::MAX)).unwrap();
        assert!(
            prefix.len() as u64 + super::FIXED_METADATA_RESERVE_BYTES
                <= MAX_CAPTURE_METADATA_BYTES - MAX_FAILURE_MARKER_BYTES
        );

        let identity_limit = MAX_CAPTURE_METADATA_BYTES
            - MAX_FAILURE_MARKER_BYTES
            - super::FIXED_METADATA_RESERVE_BYTES
            - (MAX_PROFILE_CPUS as u64 * super::PER_CPU_METADATA_RESERVE_BYTES);
        let profiler = "x".repeat(identity_limit as usize - 5);
        validate_identity_metadata(&profiler, "x").unwrap();
        let boundary = Meta {
            profiler_build: profiler,
            deployment: "x".into(),
            ..meta
        };
        let prefix = super::live_manifest_prefix(&boundary, Some(u64::MAX)).unwrap();
        assert!(
            prefix.len() as u64 + super::FIXED_METADATA_RESERVE_BYTES
                <= MAX_CAPTURE_METADATA_BYTES - MAX_FAILURE_MARKER_BYTES
        );
    }

    #[test]
    fn pending_manifest_replacement_is_atomic() {
        let capture = std::env::temp_dir().join(format!(
            "td-profiler-pending-manifest-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&capture);
        std::fs::create_dir(&capture).unwrap();
        let meta = Meta {
            profiler_build: "profiler".into(),
            deployment: "deployment".into(),
            boot_id: "boot".into(),
            start_ns: 1,
            end_ns: 2,
            wall_start_seconds: 3,
            rate_hz: 99,
            cpus: vec![0],
            coverage: vec![(0, 1, 2)],
        };
        write_pending_manifest(&capture, &meta, None).unwrap();
        let first = std::fs::read(capture.join("manifest.json")).unwrap();
        write_pending_manifest(&capture, &meta, Some(&crate::state::Analysis::default())).unwrap();
        assert!(!capture.join("manifest.json.next").exists());
        assert!(!first.is_empty());
        assert!(!std::fs::read(capture.join("manifest.json"))
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(capture).unwrap();
    }

    #[test]
    fn failed_pending_manifest_replacement_removes_the_staged_sibling() {
        let capture = std::env::temp_dir().join(format!(
            "td-profiler-pending-manifest-failure-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&capture);
        std::fs::create_dir(&capture).unwrap();
        std::fs::create_dir(capture.join("manifest.json")).unwrap();
        let meta = Meta {
            profiler_build: "profiler".into(),
            deployment: "deployment".into(),
            boot_id: "boot".into(),
            start_ns: 1,
            end_ns: 2,
            wall_start_seconds: 3,
            rate_hz: 99,
            cpus: vec![0],
            coverage: vec![(0, 1, 2)],
        };
        assert!(write_pending_manifest(&capture, &meta, None)
            .unwrap_err()
            .contains("publish pending manifest.json"));
        assert!(!capture.join("manifest.json.next").exists());
        std::fs::remove_dir_all(capture).unwrap();
    }

    #[test]
    fn offline_report_publishes_a_separate_consistent_directory() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-regenerate-test-{}",
            std::process::id()
        ));
        let capture = root.join("capture");
        std::fs::create_dir_all(&capture).unwrap();
        std::fs::write(capture.join("processes.jsonl"), b"original\n").unwrap();
        std::fs::write(
            capture.join("manifest.json"),
            b"{\"schema\":1,\"profiler_build\":\"p\",\"deployment\":\"d\",\"boot_id\":\"b\",\"monotonic_start_ns\":1,\"monotonic_end_ns\":2,\"wall_start_seconds\":3,\"rate_mode\":\"per-cpu-frequency-target\",\"configured_rate_hz\":99,\"coverage_duration_ns\":1000000000,\"effective_rate_millihz\":null,\"cpus\":[0],\"coverage\":[{\"cpu\":0,\"start_ns\":1,\"end_ns\":1000000001}],\"samples\":999}\n",
        )
        .unwrap();
        let mut writer =
            crate::raw::Writer::new(std::fs::File::create(capture.join("samples.bin")).unwrap())
                .unwrap();
        writer
            .write_event(&Event {
                time_ns: 1,
                cpu: 0,
                sequence: 0,
                pid: 7,
                tid: 7,
                kind: Kind::Task {
                    start: StartIdentity::ProcTicks(1),
                    generation: 0,
                    comm: b"worker".to_vec(),
                    valid: true,
                },
            })
            .unwrap();
        writer
            .write_event(&Event {
                time_ns: 2,
                cpu: 0,
                sequence: 1,
                pid: 7,
                tid: 7,
                kind: Kind::Sample {
                    ip: 0x1000,
                    callchain: vec![],
                },
            })
            .unwrap();
        drop(writer);

        let valid_samples = std::fs::read(capture.join("samples.bin")).unwrap();
        std::fs::write(capture.join("samples.bin"), b"broken").unwrap();
        assert!(regenerate(&capture, None).is_err());
        assert!(!capture.join("regenerated.partial").exists());
        assert!(std::fs::symlink_metadata(capture.join("regenerated.lock"))
            .unwrap()
            .is_file());
        std::fs::write(capture.join("samples.bin"), valid_samples).unwrap();

        std::fs::create_dir(capture.join("regenerated.partial")).unwrap();
        regenerate(&capture, None).unwrap();
        assert_eq!(
            std::fs::read(capture.join("processes.jsonl")).unwrap(),
            b"original\n"
        );
        let manifest = std::fs::read_to_string(capture.join("regenerated/manifest.json")).unwrap();
        assert!(manifest.contains("\"regenerated\":true"));
        assert!(manifest.contains("\"samples\":1"));
        assert!(manifest.contains("\"effective_rate_millihz\":1000"));
        assert!(manifest.contains("\"line_resolved_samples\":0"));
        assert!(manifest.contains("\"line_unresolved_samples\":1"));
        let lines = std::fs::read_to_string(capture.join("regenerated/lines.jsonl")).unwrap();
        assert!(lines.contains("\"symbol_resolved\":false"));
        assert!(lines.contains("\"line_resolved\":false"));
        let stacks = std::fs::read_to_string(capture.join("regenerated/stacks.jsonl")).unwrap();
        assert!(stacks.contains(
            "\"line_resolved\":false,\"source_file\":\"\",\"source_file_bytes\":\"\",\"source_line\":null"
        ));
        let overview = std::fs::read_to_string(capture.join("regenerated/overview.jsonl")).unwrap();
        assert!(overview.contains("\"kind\":\"process\",\"rank\":1,\"pid\":7"));
        assert!(overview
            .lines()
            .last()
            .unwrap()
            .contains("\"kind\":\"capture\",\"samples\":1"));
        assert!(regenerate(&capture, None)
            .unwrap_err()
            .contains("already exists"));
        assert!(std::fs::symlink_metadata(capture.join("regenerated.lock"))
            .unwrap()
            .is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regeneration_lock_is_kernel_exclusive_and_reusable() {
        let capture = std::env::temp_dir().join(format!(
            "td-profiler-regeneration-lock-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&capture);
        std::fs::create_dir(&capture).unwrap();
        let first = RegenerationLock::acquire(&capture, &OutputBudget::new()).unwrap();
        assert!(RegenerationLock::acquire(&capture, &OutputBudget::new())
            .err()
            .unwrap()
            .contains("already in progress"));
        first.release().unwrap();
        RegenerationLock::acquire(&capture, &OutputBudget::new())
            .unwrap()
            .release()
            .unwrap();
        std::fs::remove_dir_all(capture).unwrap();
    }

    #[test]
    fn effective_rate_uses_total_cpu_coverage_without_floats() {
        let meta = Meta {
            profiler_build: String::new(),
            deployment: String::new(),
            boot_id: String::new(),
            start_ns: 0,
            end_ns: 1_000_000_000,
            wall_start_seconds: 0,
            rate_hz: 99,
            cpus: vec![0, 1],
            coverage: vec![(0, 0, 1_000_000_000), (1, 0, 1_000_000_000)],
        };
        assert_eq!(effective_rate_millihz(&meta, 198), Some(99_000));
        assert_eq!(
            effective_rate_millihz(
                &Meta {
                    coverage: vec![],
                    ..meta
                },
                1
            ),
            None
        );
    }

    #[test]
    fn report_outputs_share_a_hard_budget_and_pin_mode() {
        let path = std::env::temp_dir().join(format!(
            "td-profiler-report-budget-test-{}",
            std::process::id()
        ));
        let budget = OutputBudget::new();
        budget.remaining.set(3);
        let mut file = output(path.clone(), &budget).unwrap();
        file.write_all(b"123").unwrap();
        assert!(file
            .write_all(b"4")
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        finish(file, "budget test").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn offline_expansions_share_a_hard_memory_budget() {
        let mut budget = ExpansionBudget::new();
        budget
            .claim(MAX_REPORT_EXPANSION_BYTES - 1, "test")
            .unwrap();
        assert_eq!(budget.remaining(), 1);
        assert!(budget.claim(2, "test").unwrap_err().contains("expand"));
        budget.release(MAX_REPORT_EXPANSION_BYTES - 1);
        assert_eq!(budget.remaining(), MAX_REPORT_EXPANSION_BYTES);
    }
}

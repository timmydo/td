use crate::contract::{
    ATTRIBUTION_FUNCTION_FRAGMENT, ATTRIBUTION_MARKER, ATTRIBUTION_SOURCE_FILE, CAPTURE_MARKER,
    MAX_EVIDENCE_WAIT_SECS,
};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CMDLINE_BYTES: u64 = 64 * 1024;
const MAX_CAPTURE_ROOT_ENTRIES: usize = 256;
const MAX_ATTRIBUTION_ROW_BYTES: u64 = 64 * 1024;
const EVIDENCE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ATTRIBUTION_WORK_SLICE: Duration = Duration::from_millis(50);
const ATTRIBUTION_REST_SLICE: Duration = Duration::from_millis(50);
const MAX_EVIDENCE_WAIT: Duration = Duration::from_secs(MAX_EVIDENCE_WAIT_SECS as u64);
const REQUIRED_FILES: [&str; 8] = [
    "manifest.json",
    "overview.jsonl",
    "processes.jsonl",
    "hotspots.jsonl",
    "lines.jsonl",
    "stacks.jsonl",
    "stacks.folded",
    "samples.bin",
];

struct CurrentCaptures {
    complete: Vec<PathBuf>,
    pending: Option<PathBuf>,
    malformed: Option<String>,
}

pub fn wait(
    root: &Path,
    timeout: Duration,
    attribution_timeout: Option<Duration>,
    uid: u32,
    gid: u32,
    attribution_cmdline_token: Option<&str>,
) -> Result<(), String> {
    validate_wait_request(root, timeout, uid, gid)?;
    if let Some(timeout) = attribution_timeout {
        validate_wait_request(root, timeout, uid, gid)?;
    }
    let require_attribution = attribution_enabled(attribution_cmdline_token)?;
    let timeout = selected_wait_timeout(
        timeout,
        attribution_timeout,
        attribution_cmdline_token.is_some(),
        require_attribution,
    )?;
    validate_mode(root, uid, gid, 0o2750, true)?;
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|e| format!("read evidence boot ID: {e}"))?;
    let boot_id = boot_id.trim();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("evidence timeout overflow")?;
    let mut diagnostic = None;
    let mut rejected = Vec::new();
    loop {
        match current_captures(root, boot_id) {
            Ok(captures) => {
                if let Some(error) = current_capture_diagnostic(&captures) {
                    diagnostic = Some(error);
                }
                for capture in captures.complete {
                    if rejected.iter().any(|path| path == &capture) {
                        continue;
                    }
                    match validate(&capture, boot_id, uid, gid, require_attribution) {
                        Ok(()) => {
                            println!("{}", evidence_marker(require_attribution));
                            return Ok(());
                        }
                        Err(error) => {
                            diagnostic = Some(error);
                            if rejected.len() < MAX_CAPTURE_ROOT_ENTRIES {
                                rejected.push(capture);
                            }
                        }
                    }
                }
            }
            Err(error) => diagnostic = Some(error),
        }
        if Instant::now() >= deadline {
            return Err(evidence_timeout_error(root, diagnostic.as_deref()));
        }
        if require_attribution {
            run_attribution_slice(ATTRIBUTION_WORK_SLICE)?;
            thread::sleep(ATTRIBUTION_REST_SLICE);
        } else {
            thread::sleep(EVIDENCE_POLL_INTERVAL);
        }
    }
}

fn selected_wait_timeout(
    ordinary: Duration,
    attribution: Option<Duration>,
    token_configured: bool,
    attribution_required: bool,
) -> Result<Duration, String> {
    if attribution.is_some() && !token_configured {
        return Err("an attribution evidence timeout requires its cmdline token".into());
    }
    Ok(if attribution_required {
        attribution.unwrap_or(ordinary)
    } else {
        ordinary
    })
}

fn current_capture_diagnostic(captures: &CurrentCaptures) -> Option<String> {
    if let Some(error) = &captures.malformed {
        return Some(error.clone());
    }
    captures.pending.as_ref().map(|path| {
        format!(
            "current-boot partial capture remains unpublished: {}",
            path.display()
        )
    })
}

fn evidence_timeout_error(root: &Path, diagnostic: Option<&str>) -> String {
    let detail = diagnostic
        .map(|error| format!("; last candidate diagnostic: {error}"))
        .unwrap_or_default();
    format!(
        "no valid completed current-boot capture appeared below {}{detail}",
        root.display(),
    )
}

fn validate_wait_request(root: &Path, timeout: Duration, uid: u32, gid: u32) -> Result<(), String> {
    if !root.is_absolute()
        || timeout.is_zero()
        || timeout > MAX_EVIDENCE_WAIT
        || uid == 0
        || gid == 0
    {
        return Err(format!(
            "evidence root must be absolute and timeout must be in 1ns..={}s",
            MAX_EVIDENCE_WAIT.as_secs()
        ));
    }
    Ok(())
}

fn evidence_marker(require_attribution: bool) -> &'static str {
    if require_attribution {
        ATTRIBUTION_MARKER
    } else {
        CAPTURE_MARKER
    }
}

fn attribution_enabled(token: Option<&str>) -> Result<bool, String> {
    let Some(token) = token else { return Ok(false) };
    if token.is_empty()
        || token.len() > 256
        || !token.is_ascii()
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err("attribution cmdline token must be 1..=256 non-whitespace ASCII bytes".into());
    }
    let mut file = fs::File::open("/proc/cmdline")
        .map_err(|error| format!("open kernel cmdline for attribution gate: {error}"))?;
    let mut cmdline = Vec::new();
    file.by_ref()
        .take(MAX_CMDLINE_BYTES.saturating_add(1))
        .read_to_end(&mut cmdline)
        .map_err(|error| format!("read kernel cmdline for attribution gate: {error}"))?;
    if cmdline.len() as u64 > MAX_CMDLINE_BYTES {
        return Err(format!(
            "kernel cmdline exceeds its {MAX_CMDLINE_BYTES}-byte attribution bound"
        ));
    }
    Ok(cmdline_has_token(&cmdline, token.as_bytes()))
}

fn cmdline_has_token(cmdline: &[u8], token: &[u8]) -> bool {
    cmdline
        .split(|byte| byte.is_ascii_whitespace())
        .any(|field| field == token)
}

fn run_attribution_slice(duration: Duration) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or("attribution workload deadline overflow")?;
    let mut state = 0x8f3f_73b5_cf1c_9adeu64;
    while Instant::now() < deadline {
        state = td_profiler_attribution_workload(state);
    }
    std::hint::black_box(state);
    Ok(())
}

const ATTRIBUTION_SOURCE_LINE_START: u64 = line!() as u64 + 2;
#[inline(never)]
fn td_profiler_attribution_workload(mut state: u64) -> u64 {
    let mut round = 0u64;
    while round != 16_384 {
        // Keep the hot instructions in this source file. Calling an inlined
        // primitive helper such as rotate_left assigns its machine code to
        // core's intrinsic source line, which makes a source-line proof depend
        // on the profiler rarely sampling the surrounding loop machinery.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state ^= round;
        round += 1;
    }
    state
}
const ATTRIBUTION_SOURCE_LINE_END: u64 = line!() as u64 - 1;

fn current_captures(root: &Path, boot_id: &str) -> Result<CurrentCaptures, String> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(root).map_err(|e| format!("read capture root {}: {e}", root.display()))?
    {
        if entries.len() >= MAX_CAPTURE_ROOT_ENTRIES {
            return Err(format!(
                "capture root exceeds its {MAX_CAPTURE_ROOT_ENTRIES}-entry evidence bound"
            ));
        }
        entries.push(entry.map_err(|e| format!("read capture root entry: {e}"))?);
    }
    let prefix = format!("{boot_id}.");
    let mut captures = Vec::new();
    let mut pending = Vec::new();
    let mut malformed = None;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || name.ends_with(".quarantine") {
            continue;
        }
        let (candidate, is_pending) = name
            .strip_suffix(".partial")
            .map_or((name, false), |candidate| (candidate, true));
        match capture_sequence(candidate, boot_id) {
            Ok(sequence) if is_pending => pending.push((sequence, entry.path())),
            Ok(sequence) => captures.push((sequence, entry.path())),
            Err(error) => malformed = Some(error),
        }
    }
    captures.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    pending.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    Ok(CurrentCaptures {
        complete: captures.into_iter().map(|(_, path)| path).collect(),
        pending: pending.into_iter().next().map(|(_, path)| path),
        malformed,
    })
}

fn capture_sequence(name: &str, boot_id: &str) -> Result<u64, String> {
    let rest = name
        .strip_prefix(&format!("{boot_id}."))
        .ok_or("capture name has the wrong boot ID")?;
    let fields: Vec<_> = rest.split('.').collect();
    if fields.len() != 3 {
        return Err(format!(
            "completed capture has a malformed identity: {name}"
        ));
    }
    let mut values = Vec::with_capacity(3);
    for field in fields {
        let value: u64 = field
            .parse()
            .map_err(|_| format!("completed capture has a malformed identity: {name}"))?;
        if value.to_string() != field {
            return Err(format!(
                "completed capture has a noncanonical identity: {name}"
            ));
        }
        values.push(value);
    }
    if values.first() == Some(&0) || values.get(1) == Some(&0) {
        return Err(format!(
            "completed capture has a zero process identity: {name}"
        ));
    }
    values
        .get(2)
        .copied()
        .ok_or_else(|| format!("completed capture has no sequence: {name}"))
}

fn validate(
    capture: &Path,
    boot_id: &str,
    uid: u32,
    gid: u32,
    require_attribution: bool,
) -> Result<(), String> {
    validate_mode(capture, uid, gid, 0o2750, true)?;
    for name in REQUIRED_FILES {
        let path = capture.join(name);
        validate_mode(&path, uid, gid, 0o640, false)?;
        if fs::symlink_metadata(&path)
            .map_err(|e| format!("stat capture file {}: {e}", path.display()))?
            .len()
            == 0
        {
            return Err(format!("capture file is empty: {}", path.display()));
        }
    }
    let path = capture.join("manifest.json");
    let mut file = fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut manifest = String::new();
    file.by_ref()
        .take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_string(&mut manifest)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if manifest.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("capture manifest exceeds its evidence bound".into());
    }
    for required in [format!("\"boot_id\":\"{boot_id}\"")] {
        if !manifest.contains(&required) {
            return Err(format!(
                "capture manifest lacks required evidence {required}"
            ));
        }
    }
    validate_integrity_counters(&manifest)?;
    let samples = number(&manifest, "\"samples\":")?;
    if samples == 0 {
        return Err("current-boot capture contains no user-mode samples".into());
    }
    if manifest.contains("\"objects\":[]") {
        return Err("current-boot capture contains no indexed store objects".into());
    }
    let complete_stack_samples = number(&manifest, "\"complete_stack_samples\":")?;
    let truncated_stack_samples = number(&manifest, "\"truncated_stack_samples\":")?;
    let unresolved_stack_samples = number(&manifest, "\"unresolved_stack_samples\":")?;
    let stack_samples = complete_stack_samples
        .checked_add(truncated_stack_samples)
        .and_then(|count| count.checked_add(unresolved_stack_samples))
        .ok_or("capture stack-sample count overflows")?;
    if stack_samples != samples {
        return Err("capture has inconsistent sample-weighted stack coverage".into());
    }
    let line_resolved_samples = number(&manifest, "\"line_resolved_samples\":")?;
    let line_unresolved_samples = number(&manifest, "\"line_unresolved_samples\":")?;
    let line_samples = line_resolved_samples
        .checked_add(line_unresolved_samples)
        .ok_or("capture line-sample count overflows")?;
    if line_samples == 0 || line_samples > samples {
        return Err("capture has inconsistent sampled-leaf line coverage".into());
    }
    validate_overview(
        capture,
        &manifest,
        samples,
        complete_stack_samples,
        truncated_stack_samples,
        unresolved_stack_samples,
        line_resolved_samples,
        line_unresolved_samples,
    )?;
    let raw_path = capture.join("samples.bin");
    let mut raw =
        fs::File::open(&raw_path).map_err(|e| format!("open {}: {e}", raw_path.display()))?;
    let mut magic = [0u8; 8];
    raw.read_exact(&mut magic)
        .map_err(|e| format!("read {} evidence magic: {e}", raw_path.display()))?;
    if &magic != b"TDPRFRAW" {
        return Err("samples.bin has the wrong raw-stream magic".into());
    }
    if require_attribution && !lines_contain_attribution(&capture.join("lines.jsonl"))? {
        return Err(format!(
            "capture has no line-resolved {ATTRIBUTION_FUNCTION_FRAGMENT} sample in \
             {ATTRIBUTION_SOURCE_FILE}:{ATTRIBUTION_SOURCE_LINE_START}..={ATTRIBUTION_SOURCE_LINE_END}"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_overview(
    capture: &Path,
    manifest: &str,
    samples: u64,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
) -> Result<(), String> {
    let path = capture.join("overview.jsonl");
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .map_err(|error| format!("open overview {}: {error}", path.display()))?
        .take(crate::report::MAX_OVERVIEW_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read overview {}: {error}", path.display()))?;
    if bytes.len() as u64 > crate::report::MAX_OVERVIEW_BYTES {
        return Err(format!(
            "capture overview exceeds its {}-byte evidence bound",
            crate::report::MAX_OVERVIEW_BYTES
        ));
    }
    let overview = std::str::from_utf8(&bytes)
        .map_err(|_| format!("overview {} is not UTF-8 JSONL", path.display()))?;
    if !overview.ends_with('\n') {
        return Err("capture overview is not newline-terminated JSONL".into());
    }
    let lines: Vec<_> = overview.lines().collect();
    let maximum = crate::report::OVERVIEW_ROWS_PER_KIND
        .saturating_mul(4)
        .saturating_add(1);
    if lines.is_empty() || lines.len() > maximum {
        return Err(format!(
            "capture overview has {} rows outside 1..={maximum}",
            lines.len()
        ));
    }
    let summary = parse_overview_summary(lines.last().copied().unwrap_or_default())?;
    for (field, actual, expected) in [
        ("samples", summary.samples, samples),
        (
            "complete_stack_samples",
            summary.complete_stack_samples,
            complete_stack_samples,
        ),
        (
            "truncated_stack_samples",
            summary.truncated_stack_samples,
            truncated_stack_samples,
        ),
        (
            "unresolved_stack_samples",
            summary.unresolved_stack_samples,
            unresolved_stack_samples,
        ),
        (
            "line_resolved_samples",
            summary.line_resolved_samples,
            line_resolved_samples,
        ),
        (
            "line_unresolved_samples",
            summary.line_unresolved_samples,
            line_unresolved_samples,
        ),
        ("lost", summary.lost, number(manifest, "\"lost\":")?),
        (
            "corrupt",
            summary.corrupt,
            number(manifest, "\"corrupt\":")?,
        ),
        (
            "omitted_errors",
            summary.omitted_errors,
            number(manifest, "\"omitted_errors\":")?,
        ),
    ] {
        if actual != expected {
            return Err(format!(
                "capture overview {field} does not match its manifest"
            ));
        }
    }

    let ranked = lines
        .get(..lines.len().saturating_sub(1))
        .unwrap_or_default();
    let mut offset = 0usize;
    let mut displayed_stack_samples = OverviewStackSamples::default();
    for (kind, total) in [
        ("process", summary.process_rows),
        ("stack", summary.stack_rows),
        ("hotspot", summary.hotspot_rows),
        ("line", summary.line_rows),
    ] {
        let shown = usize::try_from(total.min(crate::report::OVERVIEW_ROWS_PER_KIND as u64))
            .map_err(|_| "capture overview displayed-row count overflows")?;
        if shown == 0 {
            return Err(format!("capture overview has no {kind} rows"));
        }
        let end = offset
            .checked_add(shown)
            .ok_or("capture overview ranked-row count overflows")?;
        let rows = ranked
            .get(offset..end)
            .ok_or_else(|| format!("capture overview omits advertised top-ranked {kind} rows"))?;
        for (index, row) in rows.iter().enumerate() {
            if let Some((state, samples)) =
                validate_overview_row(row, kind, index.saturating_add(1), samples)?
            {
                displayed_stack_samples.add(state, samples)?;
            }
        }
        offset = end;
    }
    if offset != ranked.len() {
        return Err("capture overview contains unadvertised ranked rows".into());
    }
    let total_stack_samples = OverviewStackSamples {
        complete: summary.complete_stack_samples,
        truncated: summary.truncated_stack_samples,
        unresolved: summary.unresolved_stack_samples,
    };
    let complete_roster = summary.stack_rows <= crate::report::OVERVIEW_ROWS_PER_KIND as u64;
    if (complete_roster && displayed_stack_samples != total_stack_samples)
        || (!complete_roster && displayed_stack_samples.exceeds(&total_stack_samples))
    {
        return Err("capture overview stack rows disagree with its state totals".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum OverviewStackState {
    Complete,
    Truncated,
    Unresolved,
}

#[derive(Default, Eq, PartialEq)]
struct OverviewStackSamples {
    complete: u64,
    truncated: u64,
    unresolved: u64,
}

impl OverviewStackSamples {
    fn add(&mut self, state: OverviewStackState, samples: u64) -> Result<(), String> {
        let total = match state {
            OverviewStackState::Complete => &mut self.complete,
            OverviewStackState::Truncated => &mut self.truncated,
            OverviewStackState::Unresolved => &mut self.unresolved,
        };
        *total = total
            .checked_add(samples)
            .ok_or("capture overview displayed stack samples overflow")?;
        Ok(())
    }

    fn exceeds(&self, total: &Self) -> bool {
        self.complete > total.complete
            || self.truncated > total.truncated
            || self.unresolved > total.unresolved
    }
}

struct OverviewSummary {
    samples: u64,
    process_rows: u64,
    stack_rows: u64,
    hotspot_rows: u64,
    line_rows: u64,
    complete_stack_samples: u64,
    truncated_stack_samples: u64,
    unresolved_stack_samples: u64,
    line_resolved_samples: u64,
    line_unresolved_samples: u64,
    lost: u64,
    corrupt: u64,
    omitted_errors: u64,
}

fn parse_overview_summary(row: &str) -> Result<OverviewSummary, String> {
    let mut cursor = OverviewCursor::new(row);
    cursor.header("capture", None)?;
    let samples = cursor.named_u64("samples")?;
    let process_rows = cursor.named_u64("process_rows")?;
    let stack_rows = cursor.named_u64("stack_rows")?;
    let hotspot_rows = cursor.named_u64("hotspot_rows")?;
    let line_rows = cursor.named_u64("line_rows")?;
    let limit = cursor.named_u64("rows_per_kind_limit")?;
    if limit != crate::report::OVERVIEW_ROWS_PER_KIND as u64 {
        return Err("capture overview has a different ranked-row limit".into());
    }
    let summary = OverviewSummary {
        samples,
        process_rows,
        stack_rows,
        hotspot_rows,
        line_rows,
        complete_stack_samples: cursor.named_u64("complete_stack_samples")?,
        truncated_stack_samples: cursor.named_u64("truncated_stack_samples")?,
        unresolved_stack_samples: cursor.named_u64("unresolved_stack_samples")?,
        line_resolved_samples: cursor.named_u64("line_resolved_samples")?,
        line_unresolved_samples: cursor.named_u64("line_unresolved_samples")?,
        lost: cursor.named_u64("lost")?,
        corrupt: cursor.named_u64("corrupt")?,
        omitted_errors: cursor.named_u64("omitted_errors")?,
    };
    cursor.finish()?;
    Ok(summary)
}

fn validate_overview_row(
    row: &str,
    kind: &str,
    rank: usize,
    samples: u64,
) -> Result<Option<(OverviewStackState, u64)>, String> {
    let mut cursor = OverviewCursor::new(row);
    cursor.header(kind, Some(rank as u64))?;
    cursor.named_u64("pid")?;
    let start_kind = cursor.named_string("start_kind")?;
    if !matches!(
        start_kind.as_slice(),
        b"unknown" | b"proc-start-ticks" | b"perf-fork-time-ns"
    ) {
        return Err(format!(
            "capture overview has invalid {kind} start identity"
        ));
    }
    let start_value = cursor.named_u64("start_value")?;
    if (start_kind == b"unknown") != (start_value == 0) {
        return Err(format!("capture overview has invalid {kind} start value"));
    }
    cursor.named_u64("generation")?;
    let mut stack_state = None;
    match kind {
        "process" => {
            cursor.named_bytes("comm")?;
            cursor.named_bool("observed")?;
            cursor.named_bool("baseline_valid")?;
            cursor.named_bool("exited")?;
        }
        "stack" => {
            cursor.named_u64("tid")?;
            let state = cursor.named_string("state")?;
            let (state_name, parsed_state) = match state.as_slice() {
                b"complete" => ("complete", OverviewStackState::Complete),
                b"truncated" => ("truncated", OverviewStackState::Truncated),
                b"unresolved" => ("unresolved", OverviewStackState::Unresolved),
                _ => return Err("capture overview stack has an invalid state".into()),
            };
            stack_state = Some(parsed_state);
            let reason = cursor.named_bytes("reason")?;
            cursor.named_u64("frame_count")?;
            let folded = cursor.named_bytes("folded")?;
            let expected_prefix_length = usize::try_from(
                folded
                    .length
                    .min(crate::report::OVERVIEW_FIELD_PREFIX_BYTES as u64),
            )
            .map_err(|_| "capture overview folded prefix length overflows")?;
            if folded.length == 0
                || folded.prefix.len() != expected_prefix_length
                || folded
                    .prefix
                    .iter()
                    .any(|byte| !(0x21..=0x7e).contains(byte))
            {
                return Err("capture overview stack has an invalid folded encoding".into());
            }
            if matches!(parsed_state, OverviewStackState::Complete) {
                if reason.length != 0 || folded.prefix.starts_with(b"[td:") {
                    return Err("capture overview complete stack fields disagree".into());
                }
            } else {
                if reason.length == 0 || reason.truncated {
                    return Err("capture overview incomplete stack fields disagree".into());
                }
                let mut marker = Vec::new();
                crate::report::write_folded_marker(&mut marker, state_name, &reason.prefix)?;
                let compared = marker.len().min(folded.prefix.len());
                let marker_prefix = marker.get(..compared).unwrap_or_default();
                let folded_prefix = folded.prefix.get(..compared).unwrap_or_default();
                if folded.length < marker.len() as u64 || folded_prefix != marker_prefix {
                    return Err("capture overview incomplete stack fields disagree".into());
                }
            }
        }
        "hotspot" => {
            cursor.named_bool("resolved")?;
            cursor.named_bytes("object")?;
            cursor.named_bytes("function")?;
            cursor.named_hex("build_id", 20)?;
            cursor.named_u64("function_address")?;
        }
        "line" => {
            let symbol_resolved = cursor.named_bool("symbol_resolved")?;
            let line_resolved = cursor.named_bool("line_resolved")?;
            cursor.named_bytes("object")?;
            cursor.named_bytes("function")?;
            cursor.named_hex("build_id", 20)?;
            let function_address = cursor.named_nullable_u64("function_address")?;
            let object_address = cursor.named_nullable_u64("object_address")?;
            cursor.named_bytes("source_file")?;
            let source_line = cursor.named_nullable_u64("source_line")?;
            let source_column = cursor.named_nullable_u64("source_column")?;
            let discriminator = cursor.named_nullable_u64("discriminator")?;
            if (line_resolved && !symbol_resolved)
                || function_address.is_some() != symbol_resolved
                || object_address.is_some() == line_resolved
                || source_line.is_some() != line_resolved
                || source_column.is_some() != line_resolved
                || discriminator.is_some() != line_resolved
            {
                return Err("capture overview line resolution fields disagree".into());
            }
        }
        _ => return Err("capture overview contains an unknown ranked-row kind".into()),
    }
    let row_samples = cursor.named_u64("samples")?;
    let share = cursor.named_u64("sample_share_millionths")?;
    if share != crate::report::sample_share_millionths(row_samples, samples) {
        return Err(format!(
            "capture overview has an invalid {kind} sample share"
        ));
    }
    cursor.finish()?;
    Ok(stack_state.map(|state| (state, row_samples)))
}

struct OverviewCursor<'a> {
    rest: &'a str,
}

struct OverviewByteField {
    prefix: Vec<u8>,
    length: u64,
    truncated: bool,
}

impl<'a> OverviewCursor<'a> {
    fn new(row: &'a str) -> Self {
        Self { rest: row }
    }

    fn header(&mut self, kind: &str, rank: Option<u64>) -> Result<(), String> {
        self.literal("{\"schema\":")?;
        if self.u64()? != u64::from(crate::report::SCHEMA) {
            return Err("capture overview row has an unsupported schema".into());
        }
        if self.named_string("kind")? != kind.as_bytes() {
            return Err(format!("capture overview expected a {kind} row"));
        }
        if let Some(rank) = rank {
            if self.named_u64("rank")? != rank {
                return Err(format!("capture overview has a noncanonical {kind} rank"));
            }
        }
        Ok(())
    }

    fn named_u64(&mut self, name: &str) -> Result<u64, String> {
        self.named(name)?;
        self.u64()
    }

    fn named_nullable_u64(&mut self, name: &str) -> Result<Option<u64>, String> {
        self.named(name)?;
        if let Some(rest) = self.rest.strip_prefix("null") {
            self.rest = rest;
            Ok(None)
        } else {
            self.u64().map(Some)
        }
    }

    fn named_bool(&mut self, name: &str) -> Result<bool, String> {
        self.named(name)?;
        if let Some(rest) = self.rest.strip_prefix("true") {
            self.rest = rest;
            Ok(true)
        } else if let Some(rest) = self.rest.strip_prefix("false") {
            self.rest = rest;
            Ok(false)
        } else {
            Err(format!("capture overview {name} is not a boolean"))
        }
    }

    fn named_string(&mut self, name: &str) -> Result<Vec<u8>, String> {
        self.named(name)?;
        self.string()
    }

    fn named_hex(&mut self, name: &str, maximum_bytes: usize) -> Result<Vec<u8>, String> {
        self.named(name)?;
        self.hex(maximum_bytes)
    }

    fn named_bytes(&mut self, name: &str) -> Result<OverviewByteField, String> {
        self.literal(",")?;
        let text_name = format!("\"{name}\":");
        let text = if let Some(rest) = self.rest.strip_prefix(&text_name) {
            self.rest = rest;
            let value = self.string()?;
            self.literal(",")?;
            Some(value)
        } else {
            None
        };
        self.literal(&format!("\"{name}_bytes_prefix\":"))?;
        let prefix = self.hex(crate::report::OVERVIEW_FIELD_PREFIX_BYTES)?;
        let length = self.named_u64(&format!("{name}_bytes_length"))?;
        let truncated = self.named_bool(&format!("{name}_truncated"))?;
        let prefix_length = prefix.len() as u64;
        if truncated != (prefix_length != length) || prefix_length > length {
            return Err(format!(
                "capture overview {name} prefix length is inconsistent"
            ));
        }
        let expected_text = (!truncated)
            .then(|| std::str::from_utf8(&prefix).ok())
            .flatten()
            .map(str::as_bytes);
        if text.as_deref() != expected_text {
            return Err(format!(
                "capture overview {name} text disagrees with its byte prefix"
            ));
        }
        Ok(OverviewByteField {
            prefix,
            length,
            truncated,
        })
    }

    fn named(&mut self, name: &str) -> Result<(), String> {
        self.literal(&format!(",\"{name}\":"))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let digits = self.rest.bytes().take_while(u8::is_ascii_digit).count();
        let value = self
            .rest
            .get(..digits)
            .ok_or("capture overview has an invalid integer")?;
        if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
            return Err("capture overview has a noncanonical integer".into());
        }
        self.rest = self
            .rest
            .get(digits..)
            .ok_or("capture overview integer exceeds its row")?;
        value
            .parse()
            .map_err(|_| "capture overview integer overflows".into())
    }

    fn string(&mut self) -> Result<Vec<u8>, String> {
        self.literal("\"")?;
        let mut out = Vec::new();
        let bytes = self.rest.as_bytes();
        let mut index = 0usize;
        while let Some(byte) = bytes.get(index).copied() {
            match byte {
                b'"' => {
                    self.rest = self
                        .rest
                        .get(index.saturating_add(1)..)
                        .ok_or("capture overview string exceeds its row")?;
                    return Ok(out);
                }
                b'\\' => {
                    let escaped = bytes
                        .get(index.saturating_add(1))
                        .copied()
                        .ok_or("capture overview has an incomplete string escape")?;
                    match escaped {
                        b'"' | b'\\' | b'/' => out.push(escaped),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let end = index.saturating_add(6);
                            let digits = self
                                .rest
                                .get(index.saturating_add(2)..end)
                                .ok_or("capture overview has an incomplete Unicode escape")?;
                            let scalar = u32::from_str_radix(digits, 16)
                                .map_err(|_| "capture overview has an invalid Unicode escape")?;
                            let character = char::from_u32(scalar)
                                .ok_or("capture overview has an invalid Unicode scalar")?;
                            let mut encoded = [0u8; 4];
                            out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                            index = end;
                            continue;
                        }
                        _ => return Err("capture overview has an invalid string escape".into()),
                    }
                    index = index.saturating_add(2);
                }
                0..=0x1f => return Err("capture overview string has a control byte".into()),
                _ => {
                    out.push(byte);
                    index = index.saturating_add(1);
                }
            }
        }
        Err("capture overview has an unterminated string".into())
    }

    fn hex(&mut self, maximum_bytes: usize) -> Result<Vec<u8>, String> {
        let encoded = self.string()?;
        if encoded.len() % 2 != 0 || encoded.len() > maximum_bytes.saturating_mul(2) {
            return Err("capture overview has an invalid hexadecimal field length".into());
        }
        let mut out = Vec::with_capacity(encoded.len() / 2);
        let (pairs, _) = encoded.as_chunks::<2>();
        for [high, low] in pairs {
            let high = hex_nibble(*high)?;
            let low = hex_nibble(*low)?;
            out.push((high << 4) | low);
        }
        Ok(out)
    }

    fn literal(&mut self, literal: &str) -> Result<(), String> {
        self.rest = self
            .rest
            .strip_prefix(literal)
            .ok_or_else(|| format!("capture overview row lacks canonical {literal}"))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.literal("}")?;
        if !self.rest.is_empty() {
            return Err("capture overview row has trailing content".into());
        }
        Ok(())
    }
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("capture overview hexadecimal field is not lowercase".into()),
    }
}

fn validate_integrity_counters(manifest: &str) -> Result<(), String> {
    let lost = number(manifest, "\"lost\":")?;
    if lost != 0 {
        return Err(format!("capture reports {lost} lost perf record(s)"));
    }
    let corrupt = number(manifest, "\"corrupt\":")?;
    if corrupt != 0 {
        let errors = manifest
            .find("\"errors\":[")
            .and_then(|start| manifest.get(start..))
            .unwrap_or("errors unavailable");
        let mut end = errors.len().min(1024);
        while !errors.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        return Err(format!(
            "capture reports {corrupt} corrupt perf record(s); {}",
            errors.get(..end).unwrap_or("errors unavailable")
        ));
    }
    let omitted = number(manifest, "\"omitted_errors\":")?;
    if omitted != 0 {
        return Err(format!("capture omitted {omitted} bounded diagnostic(s)"));
    }
    validate_symbolization_errors(manifest)?;
    Ok(())
}

fn validate_symbolization_errors(manifest: &str) -> Result<(), String> {
    let marker = "\"errors\":[";
    let errors = manifest
        .find(marker)
        .and_then(|start| manifest.get(start.saturating_add(marker.len())..))
        .ok_or("capture manifest has no errors array")?;
    if errors.starts_with(']') {
        return Ok(());
    }

    // The canonical writer emits every error object with `cpu` first. Analysis
    // diagnostics carry a number; symbolization diagnostics carry null. Quotes
    // inside the byte-string message are escaped, so they cannot imitate this
    // raw object-field prefix.
    let field = "{\"cpu\":";
    let mut cursor = errors;
    let mut entries = 0usize;
    while let Some(start) = cursor.find(field) {
        entries = entries.saturating_add(1);
        cursor = cursor
            .get(start.saturating_add(field.len())..)
            .ok_or("capture manifest has a malformed errors array")?;
        if !cursor.starts_with("null,") {
            return Err("capture reports a non-symbolization analysis diagnostic".into());
        }
    }
    if entries == 0 {
        return Err("capture manifest has a malformed errors array".into());
    }
    Ok(())
}

fn lines_contain_attribution(path: &Path) -> Result<bool, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("open attribution report {}: {error}", path.display()))?;
    attribution_rows(BufReader::new(file))
        .map_err(|error| format!("read attribution report {}: {error}", path.display()))
}

fn attribution_rows(mut reader: impl BufRead) -> std::io::Result<bool> {
    let mut row = Vec::new();
    let source_field = format!("\"source_file\":\"{ATTRIBUTION_SOURCE_FILE}\"");
    loop {
        row.clear();
        let read = {
            let mut bounded = reader
                .by_ref()
                .take(MAX_ATTRIBUTION_ROW_BYTES.saturating_add(1));
            bounded.read_until(b'\n', &mut row)?
        };
        if read == 0 {
            return Ok(false);
        }
        if read as u64 > MAX_ATTRIBUTION_ROW_BYTES && !row.ends_with(b"\n") {
            discard_row_tail(&mut reader)?;
            continue;
        }
        if attribution_row(&row, source_field.as_bytes()) {
            return Ok(true);
        }
    }
}

fn discard_row_tail(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position.saturating_add(1));
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

fn attribution_row(row: &[u8], source_field: &[u8]) -> bool {
    contains(row, b"\"symbol_resolved\":true")
        && contains(row, b"\"line_resolved\":true")
        && contains(row, ATTRIBUTION_FUNCTION_FRAGMENT.as_bytes())
        && contains(row, source_field)
        && decimal(row, b"\"source_line\":").is_some_and(|line| {
            (ATTRIBUTION_SOURCE_LINE_START..=ATTRIBUTION_SOURCE_LINE_END).contains(&line)
        })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn decimal(row: &[u8], label: &[u8]) -> Option<u64> {
    let at = row
        .windows(label.len())
        .position(|window| window == label)?
        .checked_add(label.len())?;
    let mut value = 0u64;
    let mut digits = 0usize;
    let rest = row.get(at..)?;
    for byte in rest {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        digits = digits.saturating_add(1);
    }
    if digits == 0 || (digits > 1 && rest.first() == Some(&b'0')) {
        return None;
    }
    matches!(rest.get(digits), Some(b',' | b'}')).then_some(value)
}

fn validate_mode(
    path: &Path,
    uid: u32,
    gid: u32,
    expected_mode: u32,
    directory: bool,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat evidence path {}: {error}", path.display()))?;
    let expected_type = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    let mode = metadata.mode() & 0o7777;
    if !expected_type || metadata.uid() != uid || metadata.gid() != gid || mode != expected_mode {
        return Err(format!(
            "evidence path {} must be {} owned by {uid}:{gid} with mode {expected_mode:04o}; got {}:{} mode {mode:04o}",
            path.display(),
            if directory { "a directory" } else { "a regular file" },
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

fn number(text: &str, label: &str) -> Result<u64, String> {
    let rest = text
        .split_once(label)
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("capture manifest has no {label} field"))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(format!("capture manifest has invalid {label} field"));
    }
    digits
        .parse()
        .map_err(|_| format!("capture manifest {label} field overflows"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        attribution_row, attribution_rows, capture_sequence, cmdline_has_token,
        current_capture_diagnostic, current_captures, evidence_marker, number,
        selected_wait_timeout, validate_integrity_counters, validate_overview,
        validate_wait_request, wait, CurrentCaptures, ATTRIBUTION_FUNCTION_FRAGMENT,
        ATTRIBUTION_MARKER, ATTRIBUTION_SOURCE_FILE, ATTRIBUTION_SOURCE_LINE_END,
        ATTRIBUTION_SOURCE_LINE_START, CAPTURE_MARKER, MAX_ATTRIBUTION_ROW_BYTES,
    };
    use std::io::Cursor;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn manifest_numbers_are_exact_decimal_fields() {
        assert_eq!(
            number("{\"samples\":17,\"lost\":0}", "\"samples\":").unwrap(),
            17
        );
        assert!(number("{\"samples\":null}", "\"samples\":").is_err());
    }

    #[test]
    fn integrity_accepts_symbolization_diagnostics_but_not_loss_or_corruption() {
        assert!(validate_integrity_counters(
            "{\"lost\":0,\"corrupt\":0,\"omitted_errors\":0,\"errors\":[{\"cpu\":null,\"start_ns\":null,\"end_ns\":null,\"count\":1,\"message\":\"unresolved\"}]}"
        )
        .is_ok());
        assert!(validate_integrity_counters(
            "{\"lost\":2,\"corrupt\":0,\"omitted_errors\":0,\"errors\":[]}"
        )
        .unwrap_err()
        .contains("2 lost"));
        let error = validate_integrity_counters(
            "{\"lost\":0,\"corrupt\":3,\"omitted_errors\":0,\"errors\":[{\"cpu\":0,\"message\":\"bad task\"}]}",
        )
        .unwrap_err();
        assert!(error.contains("3 corrupt"));
        assert!(error.contains("bad task"));

        assert!(validate_integrity_counters(
            "{\"lost\":0,\"corrupt\":0,\"omitted_errors\":1,\"errors\":[]}"
        )
        .unwrap_err()
        .contains("omitted 1"));
        assert!(validate_integrity_counters(
            "{\"lost\":0,\"corrupt\":0,\"omitted_errors\":0,\"errors\":[{\"cpu\":0,\"message\":\"carry-forward omitted\"}]}"
        )
        .unwrap_err()
        .contains("non-symbolization"));
    }

    #[test]
    fn capture_sequences_are_numeric_and_canonical() {
        let boot = "12345678-1234-1234-1234-123456789abc";
        assert_eq!(
            capture_sequence(&format!("{boot}.7.9.10"), boot).unwrap(),
            10
        );
        assert!(capture_sequence(&format!("{boot}.7.9.010"), boot).is_err());
        assert!(capture_sequence(&format!("{boot}.7.9"), boot).is_err());
    }

    #[test]
    fn evidence_wait_has_one_exact_compiled_ceiling() {
        let root = Path::new("/var/lib/td-profiler/captures");
        assert!(validate_wait_request(root, Duration::from_secs(900), 997, 996).is_ok());
        let error = validate_wait_request(root, Duration::from_secs(901), 997, 996).unwrap_err();
        assert!(error.contains("1ns..=900s"));
        assert!(validate_wait_request(root, Duration::ZERO, 997, 996).is_err());
    }

    #[test]
    fn attribution_timeout_is_selected_only_by_the_exact_boot_token() {
        let ordinary = Duration::from_secs(300);
        let attribution = Some(Duration::from_secs(900));
        assert_eq!(
            selected_wait_timeout(ordinary, attribution, true, false).unwrap(),
            ordinary
        );
        assert_eq!(
            selected_wait_timeout(ordinary, attribution, true, true).unwrap(),
            Duration::from_secs(900)
        );
        assert!(selected_wait_timeout(ordinary, attribution, false, false)
            .unwrap_err()
            .contains("requires its cmdline token"));
    }

    #[test]
    fn completed_captures_sort_by_numeric_sequence_and_do_not_lexically_regress() {
        let boot = "12345678-1234-1234-1234-123456789abc";
        let root =
            std::env::temp_dir().join(format!("td-profiler-evidence-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        for name in [
            format!("{boot}.7.9.9"),
            format!("{boot}.7.9.10"),
            format!("{boot}.7.9.010"),
        ] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let pending_name = format!("{boot}.7.9.11.partial");
        std::fs::create_dir(root.join(&pending_name)).unwrap();
        let captures = current_captures(&root, boot).unwrap();
        assert!(captures.malformed.is_some());
        assert_eq!(
            captures
                .pending
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            pending_name
        );
        let names: Vec<_> = captures
            .complete
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [format!("{boot}.7.9.10"), format!("{boot}.7.9.9")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expiry_identifies_an_unpublished_partial_through_wait() {
        let root =
            std::env::temp_dir().join(format!("td-profiler-expiry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let metadata = std::fs::symlink_metadata(&root).unwrap();
        let uid = metadata.uid();
        let gid = metadata.gid();
        if uid == 0 || gid == 0 {
            // Root is outside the production wait contract, and a root-mapped
            // user namespace may not map any supported nonzero owner.
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2750)).unwrap();
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let pending = root.join(format!("{}.7.9.11.partial", boot.trim()));
        std::fs::create_dir(&pending).unwrap();
        let error = wait(&root, Duration::from_nanos(1), None, uid, gid, None).unwrap_err();
        assert!(error.contains("current-boot partial capture remains unpublished"));
        assert!(error.contains(&pending.to_string_lossy().into_owned()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_capture_diagnostic_precedes_an_unpublished_partial() {
        let captures = CurrentCaptures {
            complete: Vec::new(),
            pending: Some(PathBuf::from("ignored.partial")),
            malformed: Some("malformed current-boot capture".into()),
        };
        assert_eq!(
            current_capture_diagnostic(&captures).as_deref(),
            Some("malformed current-boot capture")
        );
    }

    #[test]
    fn malformed_partial_names_are_never_complete_captures() {
        let boot = "12345678-1234-1234-1234-123456789abc";
        let root = std::env::temp_dir().join(format!(
            "td-profiler-malformed-partial-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(format!("{boot}.7.9.010.partial"))).unwrap();
        let captures = current_captures(&root, boot).unwrap();
        assert!(captures.complete.is_empty());
        assert!(captures.pending.is_none());
        assert!(captures.malformed.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn attribution(line: u64) -> String {
        let function = format!("_Rabc{ATTRIBUTION_FUNCTION_FRAGMENT}def");
        format!(
            "{{\"schema\":1,\"pid\":7,\"start_kind\":\"ticks\",\"start_value\":9,\
             \"generation\":1,\"symbol_resolved\":true,\"line_resolved\":true,{},{},\
             \"build_id\":\"0011\",\"function_address\":256,\"object_address\":null,{},\
             \"source_line\":{line},\"source_column\":1,\"discriminator\":0,\"samples\":7}}\n",
            crate::json::named_bytes("object", b"/td/store/test/bin/td-profiler"),
            crate::json::named_bytes("function", function.as_bytes()),
            crate::json::named_bytes("source_file", ATTRIBUTION_SOURCE_FILE.as_bytes()),
        )
    }

    fn source_field() -> String {
        format!("\"source_file\":\"{ATTRIBUTION_SOURCE_FILE}\"")
    }

    #[test]
    fn attribution_requires_the_named_function_dwarf_source_and_function_line() {
        let valid = attribution(ATTRIBUTION_SOURCE_LINE_START);
        let source_field = source_field();
        assert!(attribution_row(valid.as_bytes(), source_field.as_bytes()));
        assert!(!attribution_row(
            attribution(ATTRIBUTION_SOURCE_LINE_START.saturating_sub(1)).as_bytes(),
            source_field.as_bytes(),
        ));
        assert!(!attribution_row(
            valid
                .replace(ATTRIBUTION_FUNCTION_FRAGMENT, "other_workload")
                .as_bytes(),
            source_field.as_bytes(),
        ));
        assert!(!attribution_row(
            valid
                .replace(ATTRIBUTION_SOURCE_FILE, "/tmp/evidence.rs")
                .as_bytes(),
            source_field.as_bytes(),
        ));
    }

    #[test]
    fn attribution_cmdline_gate_matches_one_exact_ascii_field() {
        assert!(cmdline_has_token(
            b"console=ttyS0 td.autotest=1 root=/dev/vda",
            b"td.autotest=1"
        ));
        assert!(!cmdline_has_token(
            b"console=ttyS0 not-td.autotest=1 root=/dev/vda",
            b"td.autotest=1"
        ));
    }

    #[test]
    fn normal_and_attribution_evidence_markers_remain_distinct() {
        assert_eq!(evidence_marker(false), CAPTURE_MARKER);
        assert_eq!(evidence_marker(true), ATTRIBUTION_MARKER);
        assert_ne!(CAPTURE_MARKER, ATTRIBUTION_MARKER);
    }

    #[test]
    fn overview_evidence_requires_the_complete_bounded_ranked_schema() {
        let root = std::env::temp_dir().join(format!(
            "td-profiler-overview-evidence-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let path = root.join("overview.jsonl");
        let valid = "{\"schema\":1,\"kind\":\"process\",\"rank\":1,\"pid\":7,\
                     \"start_kind\":\"proc-start-ticks\",\"start_value\":9,\
                     \"generation\":1,\"comm\":\"a\",\"comm_bytes_prefix\":\"61\",\
                     \"comm_bytes_length\":1,\"comm_truncated\":false,\"observed\":true,\
                     \"baseline_valid\":true,\"exited\":false,\"samples\":7,\
                     \"sample_share_millionths\":1000000}\n\
                     {\"schema\":1,\"kind\":\"stack\",\"rank\":1,\"pid\":7,\
                     \"start_kind\":\"proc-start-ticks\",\"start_value\":9,\
                     \"generation\":1,\"tid\":7,\"state\":\"complete\",\
                     \"reason\":\"\",\"reason_bytes_prefix\":\"\",\
                     \"reason_bytes_length\":0,\"reason_truncated\":false,\
                     \"frame_count\":1,\"folded\":\"main\",\
                     \"folded_bytes_prefix\":\"6d61696e\",\"folded_bytes_length\":4,\
                     \"folded_truncated\":false,\"samples\":7,\
                     \"sample_share_millionths\":1000000}\n\
                     {\"schema\":1,\"kind\":\"hotspot\",\"rank\":1,\"pid\":7,\
                     \"start_kind\":\"proc-start-ticks\",\"start_value\":9,\
                     \"generation\":1,\"resolved\":false,\"object\":\"\",\
                     \"object_bytes_prefix\":\"\",\"object_bytes_length\":0,\
                     \"object_truncated\":false,\"function\":\"\",\
                     \"function_bytes_prefix\":\"\",\"function_bytes_length\":0,\
                     \"function_truncated\":false,\"build_id\":\"\",\
                     \"function_address\":0,\"samples\":7,\
                     \"sample_share_millionths\":1000000}\n\
                     {\"schema\":1,\"kind\":\"line\",\"rank\":1,\"pid\":7,\
                     \"start_kind\":\"proc-start-ticks\",\"start_value\":9,\
                     \"generation\":1,\"symbol_resolved\":false,\"line_resolved\":false,\
                     \"object\":\"\",\"object_bytes_prefix\":\"\",\
                     \"object_bytes_length\":0,\"object_truncated\":false,\
                     \"function\":\"\",\"function_bytes_prefix\":\"\",\
                     \"function_bytes_length\":0,\"function_truncated\":false,\
                     \"build_id\":\"\",\"function_address\":null,\
                     \"object_address\":0,\"source_file\":\"\",\
                     \"source_file_bytes_prefix\":\"\",\"source_file_bytes_length\":0,\
                     \"source_file_truncated\":false,\"source_line\":null,\
                     \"source_column\":null,\"discriminator\":null,\"samples\":7,\
                     \"sample_share_millionths\":1000000}\n\
                     {\"schema\":1,\"kind\":\"capture\",\"samples\":7,\
                     \"process_rows\":1,\"stack_rows\":1,\"hotspot_rows\":1,\"line_rows\":1,\
                     \"rows_per_kind_limit\":32,\"complete_stack_samples\":7,\
                     \"truncated_stack_samples\":0,\"unresolved_stack_samples\":0,\
                     \"line_resolved_samples\":4,\"line_unresolved_samples\":3,\
                     \"lost\":0,\"corrupt\":0,\"omitted_errors\":0}\n";
        std::fs::write(&path, valid).unwrap();
        let manifest = "{\"lost\":0,\"corrupt\":0,\"omitted_errors\":0}";
        validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3).unwrap();
        assert!(validate_overview(&root, manifest, 8, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("samples does not match"));

        std::fs::write(
            &path,
            valid.replace(
                "{\"schema\":1,\"kind\":\"process\",\"rank\":1,\"pid\":7",
                "{\"schema\":1,\"kind\":\"process\",\"rank\":1,\"x\":0,\"pid\":7",
            ),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("canonical"));

        std::fs::write(
            &path,
            valid.replace("\"process_rows\":1", "\"process_rows\":2"),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("expected a process row"));

        std::fs::write(
            &path,
            valid.replace("\"state\":\"complete\"", "\"state\":\"truncated\""),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("incomplete stack fields disagree"));

        std::fs::write(
            &path,
            valid
                .replace(
                    "\"folded_bytes_prefix\":\"6d61696e\"",
                    "\"folded_bytes_prefix\":\"206d61696e\"",
                )
                .replace("\"folded\":\"main\"", "\"folded\":\" main\"")
                .replace("\"folded_bytes_length\":4", "\"folded_bytes_length\":5"),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("invalid folded encoding"));

        let unresolved = valid
            .replace("\"state\":\"complete\"", "\"state\":\"unresolved\"")
            .replace(
                "\"reason\":\"\",\"reason_bytes_prefix\":\"\",\
                 \"reason_bytes_length\":0,\"reason_truncated\":false",
                "\"reason\":\"alpha\",\"reason_bytes_prefix\":\"616c706861\",\
                 \"reason_bytes_length\":5,\"reason_truncated\":false",
            )
            .replace(
                "\"folded\":\"main\",\
                 \"folded_bytes_prefix\":\"6d61696e\",\"folded_bytes_length\":4",
                "\"folded\":\"[td:unresolved:alpha];main\",\
                 \"folded_bytes_prefix\":\
                 \"5b74643a756e7265736f6c7665643a616c7068615d3b6d61696e\",\
                 \"folded_bytes_length\":26",
            )
            .replace(
                "\"complete_stack_samples\":7",
                "\"complete_stack_samples\":0",
            )
            .replace(
                "\"unresolved_stack_samples\":0",
                "\"unresolved_stack_samples\":7",
            );
        std::fs::write(&path, &unresolved).unwrap();
        validate_overview(&root, manifest, 7, 0, 0, 7, 4, 3).unwrap();

        std::fs::write(
            &path,
            unresolved
                .replace("[td:unresolved:alpha];main", "[td:unresolved:beta];main")
                .replace(
                    "5b74643a756e7265736f6c7665643a616c7068615d3b6d61696e",
                    "5b74643a756e7265736f6c7665643a626574615d3b6d61696e",
                )
                .replace("\"folded_bytes_length\":26", "\"folded_bytes_length\":25"),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 0, 0, 7, 4, 3)
            .unwrap_err()
            .contains("incomplete stack fields disagree"));

        for (changed, expected) in [
            (
                valid.replace("\"state\":\"complete\"", "\"state\":\"invalid\""),
                "invalid state",
            ),
            (
                valid
                    .replace("\"folded\":\"main\",", "\"folded\":\"\",")
                    .replace("\"folded_bytes_prefix\":\"6d61696e\"", "\"folded_bytes_prefix\":\"\"")
                    .replace("\"folded_bytes_length\":4", "\"folded_bytes_length\":0"),
                "invalid folded encoding",
            ),
            (
                valid
                    .replace("\"folded\":\"main\"", "\"folded\":\"[td:complete:fake];main\"")
                    .replace(
                        "\"folded_bytes_prefix\":\"6d61696e\"",
                        "\"folded_bytes_prefix\":\"5b74643a636f6d706c6574653a66616b655d3b6d61696e\"",
                    )
                    .replace("\"folded_bytes_length\":4", "\"folded_bytes_length\":23"),
                "complete stack fields disagree",
            ),
            (
                valid.replace("\"stack_rows\":1", "\"stack_rows\":2"),
                "expected a stack row",
            ),
        ] {
            std::fs::write(&path, changed).unwrap();
            assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
                .unwrap_err()
                .contains(expected));
        }

        std::fs::write(
            &path,
            valid
                .replace(
                    "\"complete_stack_samples\":7",
                    "\"complete_stack_samples\":6",
                )
                .replace(
                    "\"unresolved_stack_samples\":0",
                    "\"unresolved_stack_samples\":1",
                ),
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 6, 0, 1, 4, 3)
            .unwrap_err()
            .contains("stack rows disagree with its state totals"));

        std::fs::write(
            &path,
            vec![b'x'; crate::report::MAX_OVERVIEW_BYTES as usize + 1],
        )
        .unwrap();
        assert!(validate_overview(&root, manifest, 7, 7, 0, 0, 4, 3)
            .unwrap_err()
            .contains("exceeds its"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attribution_scan_skips_an_oversized_hostile_row_without_unbounded_allocation() {
        let mut input = vec![b'x'; MAX_ATTRIBUTION_ROW_BYTES as usize + 1];
        input.extend_from_slice(b"tail\n");
        input.extend_from_slice(attribution(ATTRIBUTION_SOURCE_LINE_END).as_bytes());
        assert!(attribution_rows(Cursor::new(input)).unwrap());
    }
}

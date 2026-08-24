use crate::contract::{
    ATTRIBUTION_FUNCTION_FRAGMENT, ATTRIBUTION_MARKER, ATTRIBUTION_SOURCE_FILE, CAPTURE_MARKER,
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
const REQUIRED_FILES: [&str; 7] = [
    "manifest.json",
    "processes.jsonl",
    "hotspots.jsonl",
    "lines.jsonl",
    "stacks.jsonl",
    "stacks.folded",
    "samples.bin",
];

pub fn wait(
    root: &Path,
    timeout: Duration,
    uid: u32,
    gid: u32,
    attribution_cmdline_token: Option<&str>,
) -> Result<(), String> {
    if !root.is_absolute()
        || timeout.is_zero()
        || timeout > Duration::from_secs(300)
        || uid == 0
        || gid == 0
    {
        return Err("evidence root must be absolute and timeout must be in 1ns..=300s".into());
    }
    let require_attribution = attribution_enabled(attribution_cmdline_token)?;
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
            Ok((captures, malformed)) => {
                if let Some(error) = malformed {
                    diagnostic = Some(error);
                }
                for capture in captures {
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
            let detail = diagnostic
                .as_deref()
                .map(|error| format!("; last candidate diagnostic: {error}"))
                .unwrap_or_default();
            return Err(format!(
                "no valid completed current-boot capture appeared below {}{detail}",
                root.display(),
            ));
        }
        if require_attribution {
            run_attribution_slice(ATTRIBUTION_WORK_SLICE)?;
            thread::sleep(ATTRIBUTION_REST_SLICE);
        } else {
            thread::sleep(EVIDENCE_POLL_INTERVAL);
        }
    }
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
        state = state.rotate_left(13).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ round;
        round += 1;
    }
    state
}
const ATTRIBUTION_SOURCE_LINE_END: u64 = line!() as u64 - 1;

fn current_captures(root: &Path, boot_id: &str) -> Result<(Vec<PathBuf>, Option<String>), String> {
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
    let mut malformed = None;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || name.ends_with(".partial") || name.ends_with(".quarantine")
        {
            continue;
        }
        match capture_sequence(name, boot_id) {
            Ok(sequence) => captures.push((sequence, entry.path())),
            Err(error) => malformed = Some(error),
        }
    }
    captures.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    Ok((
        captures.into_iter().map(|(_, path)| path).collect(),
        malformed,
    ))
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
    for required in [
        format!("\"boot_id\":\"{boot_id}\""),
        "\"lost\":0".into(),
        "\"corrupt\":0".into(),
        "\"errors\":[]".into(),
    ] {
        if !manifest.contains(&required) {
            return Err(format!(
                "capture manifest lacks required evidence {required}"
            ));
        }
    }
    let samples = number(&manifest, "\"samples\":")?;
    if samples == 0 {
        return Err("current-boot capture contains no user-mode samples".into());
    }
    if manifest.contains("\"objects\":[]") {
        return Err("current-boot capture contains no indexed store objects".into());
    }
    let line_samples = number(&manifest, "\"line_resolved_samples\":")?
        .checked_add(number(&manifest, "\"line_unresolved_samples\":")?)
        .ok_or("capture line-sample count overflows")?;
    if line_samples == 0 || line_samples > samples {
        return Err("capture has inconsistent sampled-leaf line coverage".into());
    }
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
        attribution_row, attribution_rows, capture_sequence, cmdline_has_token, current_captures,
        evidence_marker, number, ATTRIBUTION_FUNCTION_FRAGMENT, ATTRIBUTION_MARKER,
        ATTRIBUTION_SOURCE_FILE, ATTRIBUTION_SOURCE_LINE_END, ATTRIBUTION_SOURCE_LINE_START,
        CAPTURE_MARKER, MAX_ATTRIBUTION_ROW_BYTES,
    };
    use std::io::Cursor;

    #[test]
    fn manifest_numbers_are_exact_decimal_fields() {
        assert_eq!(
            number("{\"samples\":17,\"lost\":0}", "\"samples\":").unwrap(),
            17
        );
        assert!(number("{\"samples\":null}", "\"samples\":").is_err());
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
        let (captures, malformed) = current_captures(&root, boot).unwrap();
        assert!(malformed.is_some());
        let names: Vec<_> = captures
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [format!("{boot}.7.9.10"), format!("{boot}.7.9.9")]);
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
    fn attribution_scan_skips_an_oversized_hostile_row_without_unbounded_allocation() {
        let mut input = vec![b'x'; MAX_ATTRIBUTION_ROW_BYTES as usize + 1];
        input.extend_from_slice(b"tail\n");
        input.extend_from_slice(attribution(ATTRIBUTION_SOURCE_LINE_END).as_bytes());
        assert!(attribution_rows(Cursor::new(input)).unwrap());
    }
}

use crate::contract::CAPTURE_MARKER;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CAPTURE_ROOT_ENTRIES: usize = 256;
const REQUIRED_FILES: [&str; 6] = [
    "manifest.json",
    "processes.jsonl",
    "hotspots.jsonl",
    "stacks.jsonl",
    "stacks.folded",
    "samples.bin",
];

pub fn wait(root: &Path, timeout: Duration, uid: u32, gid: u32) -> Result<(), String> {
    if !root.is_absolute()
        || timeout.is_zero()
        || timeout > Duration::from_secs(300)
        || uid == 0
        || gid == 0
    {
        return Err("evidence root must be absolute and timeout must be in 1ns..=300s".into());
    }
    validate_mode(root, uid, gid, 0o2750, true)?;
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|e| format!("read evidence boot ID: {e}"))?;
    let boot_id = boot_id.trim();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("evidence timeout overflow")?;
    let mut diagnostic = None;
    loop {
        match current_captures(root, boot_id) {
            Ok((captures, malformed)) => {
                if let Some(error) = malformed {
                    diagnostic = Some(error);
                }
                for capture in captures {
                    match validate(&capture, boot_id, uid, gid) {
                        Ok(()) => {
                            println!("{CAPTURE_MARKER}");
                            return Ok(());
                        }
                        Err(error) => diagnostic = Some(error),
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
        thread::sleep(Duration::from_millis(100));
    }
}

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

fn validate(capture: &Path, boot_id: &str, uid: u32, gid: u32) -> Result<(), String> {
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
    if number(&manifest, "\"samples\":")? == 0 {
        return Err("current-boot capture contains no user-mode samples".into());
    }
    if manifest.contains("\"objects\":[]") {
        return Err("current-boot capture contains no indexed store objects".into());
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
    Ok(())
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
    use super::{capture_sequence, current_captures, number};

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
}

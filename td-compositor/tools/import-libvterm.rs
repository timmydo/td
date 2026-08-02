#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[allow(dead_code)]
#[path = "../../engine/src/sha256.rs"]
mod sha256;

const RELEASE: &str = "libvterm-0.3.3";
const TAG_COMMIT: &str = "9d6d2112335080312ef8c36667fa717ded4f7daf";
const ARCHIVE_URL: &str = "https://github.com/neovim/libvterm/archive/refs/tags/v0.3.3.tar.gz";
const ARCHIVE_SHA256: &str = "0babe3ab42c354925dadede90d352f054aa9c4ae6842ea803a20c9741e172e56";
const SOURCE_MANIFEST: &str = include_str!("libvterm-0.3.3.sources");
const NATIVE_FILE: &str = "libvterm-0.3.3.term";
const REPORT_FILE: &str = "libvterm-0.3.3.report";
const MAX_SOURCE_FILE_BYTES: u64 = 1_048_576;
const MAX_SOURCE_TOTAL_BYTES: u64 = 8_388_608;

const COMMANDS: &[&str] = &[
    "DAMAGEFLUSH",
    "DAMAGEMERGE",
    "ENCIN",
    "FOCUS",
    "INCHAR",
    "INIT",
    "INKEY",
    "MOUSEBTN",
    "MOUSEMOVE",
    "PASTE",
    "PUSH",
    "RESET",
    "RESIZE",
    "SELECTION",
    "SETDEFAULTCOL",
    "UTF8",
    "WANTENCODING",
    "WANTPARSER",
    "WANTSCREEN",
    "WANTSTATE",
];

const ASSERTIONS: &[&str] = &[
    "apc",
    "control",
    "csi",
    "cursor",
    "damage",
    "dcs",
    "encout",
    "erase",
    "escape",
    "lineinfo",
    "movecursor",
    "moverect",
    "osc",
    "output",
    "pen",
    "pm",
    "putglyph",
    "sb_clear",
    "sb_popline",
    "sb_pushline",
    "screen_attrs_extent",
    "screen_cell",
    "screen_chars",
    "screen_eol",
    "screen_row",
    "screen_text",
    "scrollrect",
    "selection-query",
    "selection-set",
    "settermprop",
    "sos",
    "text",
];

#[derive(Clone, Debug)]
struct SourceLine {
    number: usize,
    text: String,
}

#[derive(Clone, Debug)]
struct SourceCase {
    number: usize,
    title: String,
    lines: Vec<SourceLine>,
}

#[derive(Clone, Debug)]
struct ParsedFile {
    path: String,
    setup: Vec<SourceLine>,
    cases: Vec<SourceCase>,
    raw_assertions: usize,
    expanded_assertions: usize,
}

#[derive(Clone, Debug)]
enum Operation {
    Write(Vec<u8>),
    Resize(usize, usize),
}

#[derive(Clone, Debug)]
struct ReplayOperation {
    operation: Operation,
    reply_after: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
enum NativeStep {
    Operation(Operation),
    Expect(String),
}

#[derive(Clone, Debug)]
struct NativeCase {
    id: String,
    source: String,
    tags: Vec<&'static str>,
    rows: usize,
    columns: usize,
    steps: Vec<NativeStep>,
}

#[derive(Default)]
struct Counts {
    source_files: usize,
    source_cases: usize,
    raw_assertions: usize,
    expanded_assertions: usize,
    native_cases: usize,
    converted_assertions: usize,
    excluded_cases: BTreeMap<String, usize>,
    excluded_assertions: BTreeMap<String, usize>,
    excluded_case_records: Vec<(String, String)>,
}

pub(crate) struct Generated {
    pub(crate) native: String,
    pub(crate) report: String,
}

fn fail(message: impl Into<String>) -> Result<(), String> {
    Err(message.into())
}

fn usage() -> String {
    "usage: td-term-import-libvterm generate SOURCE-TREE OUTPUT-DIR | \
     td-term-import-libvterm check SOURCE-TREE OUTPUT-DIR"
        .into()
}

fn os_path(value: Option<&OsString>, what: &str) -> Result<PathBuf, String> {
    value
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {what}: {}", usage()))
}

fn run() -> Result<(), String> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let mode = arguments
        .get(1)
        .and_then(|value| value.to_str())
        .ok_or_else(usage)?;
    let source = os_path(arguments.get(2), "source tree")?;
    let output = os_path(arguments.get(3), "output directory")?;
    if arguments.len() != 4 {
        return fail(usage());
    }
    let generated = generate_tree(&source)?;
    match mode {
        "generate" => {
            std::fs::create_dir_all(&output)
                .map_err(|error| format!("create {}: {error}", output.display()))?;
            write_outputs(&output, &generated)
        }
        "check" => {
            check_output(&output.join(NATIVE_FILE), &generated.native)?;
            check_output(&output.join(REPORT_FILE), &generated.report)
        }
        _ => fail(usage()),
    }
}

fn temporary_output_path(output: &Path, name: &str) -> PathBuf {
    output.join(format!(".{name}.tmp-{}", std::process::id()))
}

fn backup_output_path(output: &Path, name: &str) -> PathBuf {
    output.join(format!(".{name}.old-{}", std::process::id()))
}

fn stage_output(path: &Path, content: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn remove_staged(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn read_expected_output(path: &Path, expected_length: usize) -> Result<Vec<u8>, String> {
    let limit = expected_length
        .checked_add(1)
        .ok_or_else(|| format!("expected output size overflows for {}", path.display()))?;
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut actual = Vec::new();
    actual
        .try_reserve(limit)
        .map_err(|_| format!("reserve {limit} bytes to check {}", path.display()))?;
    file.take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut actual)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(actual)
}

fn validate_staged(path: &Path, expected: &str) -> Result<(), String> {
    let actual = read_expected_output(path, expected.len())?;
    if actual == expected.as_bytes() {
        Ok(())
    } else {
        fail(format!(
            "staged output {} did not round trip",
            path.display()
        ))
    }
}

fn require_absent(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
        Ok(_) => fail(format!(
            "{} already exists; preserve it for recovery",
            path.display()
        )),
    }
}

fn move_output_to_backup(path: &Path, backup: &Path) -> Result<bool, String> {
    match std::fs::rename(path, backup) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "move {} to recovery path {}: {error}",
            path.display(),
            backup.display()
        )),
    }
}

fn restore_output(path: &Path, backup: &Path, existed: bool) -> Result<(), String> {
    if existed {
        std::fs::rename(backup, path).map_err(|error| {
            format!(
                "restore {} from {}: {error}",
                path.display(),
                backup.display()
            )
        })
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "remove newly generated {}: {error}",
                path.display()
            )),
        }
    }
}

fn rollback_outputs(
    native: (&Path, &Path, bool),
    report: (&Path, &Path, bool),
) -> Result<(), String> {
    let mut errors = Vec::new();
    for (path, backup, existed) in [native, report] {
        if let Err(error) = restore_output(path, backup, existed) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        fail(errors.join("; "))
    }
}

fn write_outputs(output: &Path, generated: &Generated) -> Result<(), String> {
    let native = output.join(NATIVE_FILE);
    let report = output.join(REPORT_FILE);
    let native_staged = temporary_output_path(output, NATIVE_FILE);
    let report_staged = temporary_output_path(output, REPORT_FILE);
    let native_backup = backup_output_path(output, NATIVE_FILE);
    let report_backup = backup_output_path(output, REPORT_FILE);
    require_absent(&native_backup)?;
    require_absent(&report_backup)?;
    if let Err(error) = stage_output(&native_staged, &generated.native) {
        remove_staged(&native_staged);
        return Err(error);
    }
    if let Err(error) = stage_output(&report_staged, &generated.report) {
        remove_staged(&native_staged);
        remove_staged(&report_staged);
        return Err(error);
    }
    if let Err(error) = validate_staged(&native_staged, &generated.native)
        .and_then(|()| validate_staged(&report_staged, &generated.report))
    {
        remove_staged(&native_staged);
        remove_staged(&report_staged);
        return Err(error);
    }
    let native_existed = match move_output_to_backup(&native, &native_backup) {
        Ok(existed) => existed,
        Err(error) => {
            remove_staged(&native_staged);
            remove_staged(&report_staged);
            return Err(error);
        }
    };
    let report_existed = match move_output_to_backup(&report, &report_backup) {
        Ok(existed) => existed,
        Err(error) => {
            remove_staged(&native_staged);
            remove_staged(&report_staged);
            let rollback = restore_output(&native, &native_backup, native_existed);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => fail(format!(
                    "{error}; rollback failed and recovery file was preserved: {rollback_error}"
                )),
            };
        }
    };
    if let Err(error) = std::fs::rename(&native_staged, &native) {
        remove_staged(&native_staged);
        remove_staged(&report_staged);
        let rollback = rollback_outputs(
            (&native, &native_backup, native_existed),
            (&report, &report_backup, report_existed),
        );
        return match rollback {
            Ok(()) => fail(format!(
                "replace {} with staged output: {error}; restored prior outputs",
                native.display()
            )),
            Err(rollback_error) => fail(format!(
                "replace {} with staged output: {error}; rollback failed and recovery files were preserved: {rollback_error}",
                native.display()
            )),
        };
    }
    if let Err(error) = std::fs::rename(&report_staged, &report) {
        remove_staged(&report_staged);
        let rollback = rollback_outputs(
            (&native, &native_backup, native_existed),
            (&report, &report_backup, report_existed),
        );
        return match rollback {
            Ok(()) => fail(format!(
                "replace {} with staged output: {error}; restored prior outputs",
                report.display()
            )),
            Err(rollback_error) => fail(format!(
                "replace {} with staged output: {error}; rollback failed and recovery files were preserved: {rollback_error}",
                report.display()
            )),
        };
    }
    remove_staged(&native_backup);
    remove_staged(&report_backup);
    Ok(())
}

fn check_output(path: &Path, expected: &str) -> Result<(), String> {
    let actual = read_expected_output(path, expected.len())?;
    if actual == expected.as_bytes() {
        Ok(())
    } else {
        fail(format!(
            "{} is stale; rerun the importer in generate mode",
            path.display()
        ))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-term-import-libvterm: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_manifest() -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    let mut paths = BTreeSet::new();
    for (offset, raw) in SOURCE_MANIFEST.lines().enumerate() {
        let line = offset.saturating_add(1);
        let Some((digest, path)) = raw.split_once("  ") else {
            return Err(format!("source manifest:{line}: expected digest and path"));
        };
        if digest.len() != 64 || !digest.chars().all(|value| value.is_ascii_hexdigit()) {
            return Err(format!("source manifest:{line}: invalid SHA-256"));
        }
        if !paths.insert(path) {
            return Err(format!("source manifest:{line}: duplicate {path}"));
        }
        entries.push((path.to_string(), digest.to_string()));
    }
    if entries.is_empty() {
        return Err("source manifest is empty".into());
    }
    Ok(entries)
}

fn source_file_size(path: &Path) -> Result<u64, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_SOURCE_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_SOURCE_FILE_BYTES}-byte source limit",
            path.display()
        ));
    }
    Ok(metadata.len())
}

fn read_source_file(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SOURCE_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_SOURCE_FILE_BYTES}-byte source limit",
            path.display()
        ));
    }
    Ok(bytes)
}

fn load_sources(root: &Path) -> Result<Vec<(String, String)>, String> {
    let manifest = parse_manifest()?;
    let expected_paths = manifest
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let directory = root.join("t");
    let mut actual_paths = BTreeSet::new();
    for entry in std::fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("test") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("{} has a non-UTF-8 name", path.display()))?;
        actual_paths.insert(format!("t/{name}"));
    }
    if actual_paths != expected_paths {
        let missing = expected_paths
            .difference(&actual_paths)
            .cloned()
            .collect::<Vec<_>>();
        let extra = actual_paths
            .difference(&expected_paths)
            .cloned()
            .collect::<Vec<_>>();
        return Err(format!(
            "source test inventory differs from the pin; missing {missing:?}, extra {extra:?}"
        ));
    }
    let mut sources = Vec::new();
    let mut total_size = 0u64;
    for (relative, expected) in manifest {
        let path = root.join(&relative);
        let size = source_file_size(&path)?;
        let bytes = read_source_file(&path)?;
        let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        total_size = total_size
            .checked_add(size.max(actual_size))
            .filter(|total| *total <= MAX_SOURCE_TOTAL_BYTES)
            .ok_or_else(|| {
                format!("source tree exceeds the {MAX_SOURCE_TOTAL_BYTES}-byte total limit")
            })?;
        let actual = sha256::hex_digest(&bytes);
        if actual != expected {
            return Err(format!(
                "{}: expected SHA-256 {expected}, got {actual}",
                path.display()
            ));
        }
        let source =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        sources.push((relative, source));
    }
    Ok(sources)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    sha256::hex_digest(bytes)
}

fn assertion_name(text: &str) -> Option<&str> {
    let text = text.strip_prefix('?').unwrap_or(text);
    text.split_whitespace().next()
}

fn command_name(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}

fn is_assertion(text: &str) -> bool {
    text.starts_with('?')
        || text
            .chars()
            .next()
            .is_some_and(|value| value.is_ascii_lowercase())
}

fn checked_source_line(number: usize, text: String) -> Result<SourceLine, String> {
    if is_assertion(&text) {
        let name =
            assertion_name(&text).ok_or_else(|| format!("line {number}: assertion has no name"))?;
        if !ASSERTIONS.contains(&name) {
            return Err(format!("line {number}: unknown assertion {name:?}"));
        }
    } else {
        let name =
            command_name(&text).ok_or_else(|| format!("line {number}: command has no name"))?;
        if !COMMANDS.contains(&name) {
            return Err(format!("line {number}: unknown command {name:?}"));
        }
    }
    Ok(SourceLine { number, text })
}

fn replace_sequence_value(text: &str, value: usize) -> String {
    text.replace("\\#", &value.to_string())
}

fn expand_control_line(number: usize, text: &str) -> Result<Vec<SourceLine>, String> {
    if let Some(rest) = text.strip_prefix("$REP ") {
        let Some((count, inner)) = rest.split_once(':') else {
            return Err(format!("line {number}: malformed $REP"));
        };
        let count = count
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("line {number}: invalid $REP count"))?;
        if count > 1_024 {
            return Err(format!("line {number}: $REP count exceeds 1024"));
        }
        let mut output = Vec::with_capacity(count);
        for _ in 0..count {
            output.push(checked_source_line(number, inner.trim().to_string())?);
        }
        return Ok(output);
    }
    if let Some(rest) = text.strip_prefix("$SEQ ") {
        let Some((range, inner)) = rest.split_once(':') else {
            return Err(format!("line {number}: malformed $SEQ"));
        };
        let mut values = range.split_whitespace();
        let low = values
            .next()
            .ok_or_else(|| format!("line {number}: $SEQ has no lower bound"))?
            .parse::<usize>()
            .map_err(|_| format!("line {number}: invalid $SEQ lower bound"))?;
        let high = values
            .next()
            .ok_or_else(|| format!("line {number}: $SEQ has no upper bound"))?
            .parse::<usize>()
            .map_err(|_| format!("line {number}: invalid $SEQ upper bound"))?;
        if values.next().is_some() || high < low || high.saturating_sub(low) > 1_024 {
            return Err(format!("line {number}: invalid $SEQ range"));
        }
        let mut output = Vec::new();
        for value in low..=high {
            output.push(checked_source_line(
                number,
                replace_sequence_value(inner.trim(), value),
            )?);
        }
        return Ok(output);
    }
    Ok(vec![checked_source_line(number, text.to_string())?])
}

fn parse_source_file(path: &str, input: &str) -> Result<ParsedFile, String> {
    let mut setup = Vec::new();
    let mut cases = Vec::new();
    let mut current: Option<SourceCase> = None;
    let mut raw_assertions = 0usize;
    let mut expanded_assertions = 0usize;
    for (offset, raw) in input.lines().enumerate() {
        let number = offset.saturating_add(1);
        let text = raw.trim_start();
        if text == "__END__" {
            break;
        }
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        if let Some(title) = text.strip_prefix('!') {
            if let Some(case) = current.take() {
                cases.push(case);
            }
            let title = title.trim();
            if title.is_empty() {
                return Err(format!("{path}:{number}: empty case title"));
            }
            current = Some(SourceCase {
                number,
                title: title.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        if is_assertion(text) {
            raw_assertions = raw_assertions.saturating_add(1);
        } else if !text.starts_with('$') {
            let name = command_name(text)
                .ok_or_else(|| format!("{path}:{number}: command has no name"))?;
            if !COMMANDS.contains(&name) {
                return Err(format!("{path}:{number}: unknown command {name:?}"));
            }
        }
        let expanded =
            expand_control_line(number, text).map_err(|error| format!("{path}:{error}"))?;
        expanded_assertions = expanded_assertions.saturating_add(
            expanded
                .iter()
                .filter(|line| is_assertion(&line.text))
                .count(),
        );
        if let Some(case) = current.as_mut() {
            case.lines.extend(expanded);
        } else {
            setup.extend(expanded);
        }
    }
    if let Some(case) = current {
        cases.push(case);
    }
    if cases.is_empty() {
        return Err(format!("{path}: contains no named cases"));
    }
    Ok(ParsedFile {
        path: path.to_string(),
        setup,
        cases,
        raw_assertions,
        expanded_assertions,
    })
}

fn parse_sources(sources: Vec<(String, String)>) -> Result<Vec<ParsedFile>, String> {
    let mut parsed = Vec::with_capacity(sources.len());
    for (path, source) in sources {
        parsed.push(parse_source_file(&path, &source)?);
    }
    Ok(parsed)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value.saturating_sub(b'0')),
        b'a'..=b'f' => Some(value.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(value.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

fn parse_hex_byte(input: &[u8], offset: &mut usize) -> Result<u8, String> {
    let high = input
        .get(*offset)
        .and_then(|value| hex_value(*value))
        .ok_or_else(|| "\\x requires two hexadecimal digits".to_string())?;
    *offset = offset.saturating_add(1);
    let low = input
        .get(*offset)
        .and_then(|value| hex_value(*value))
        .ok_or_else(|| "\\x requires two hexadecimal digits".to_string())?;
    *offset = offset.saturating_add(1);
    Ok(high.saturating_mul(16).saturating_add(low))
}

fn parse_braced_hex(input: &[u8], offset: &mut usize) -> Result<u8, String> {
    let mut value = 0u16;
    let mut digits = 0usize;
    loop {
        let byte = input
            .get(*offset)
            .copied()
            .ok_or_else(|| "unterminated \\x{...} escape".to_string())?;
        *offset = offset.saturating_add(1);
        if byte == b'}' {
            break;
        }
        let digit =
            hex_value(byte).ok_or_else(|| "invalid hexadecimal digit in \\x{...}".to_string())?;
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(u16::from(digit)))
            .ok_or_else(|| "\\x{...} escape exceeds a byte".to_string())?;
        digits = digits.saturating_add(1);
    }
    if digits == 0 || value > u16::from(u8::MAX) {
        return Err("\\x{...} escape is not one byte".into());
    }
    u8::try_from(value).map_err(|_| "\\x{...} escape is not one byte".into())
}

fn parse_quoted(input: &str) -> Result<(Vec<u8>, &str), String> {
    let input = input.trim_start();
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'"') {
        return Err(format!("expected quoted string in {input:?}"));
    }
    let mut offset = 1usize;
    let mut output = Vec::new();
    loop {
        let byte = bytes
            .get(offset)
            .copied()
            .ok_or_else(|| "unterminated quoted string".to_string())?;
        offset = offset.saturating_add(1);
        match byte {
            b'"' => {
                let rest = input
                    .get(offset..)
                    .ok_or_else(|| "quoted string escaped input".to_string())?;
                return Ok((output, rest));
            }
            b'\\' => {
                let escaped = bytes
                    .get(offset)
                    .copied()
                    .ok_or_else(|| "quoted string ends in a backslash".to_string())?;
                offset = offset.saturating_add(1);
                match escaped {
                    b'"' => output.push(b'"'),
                    b'$' => output.push(b'$'),
                    b'0' => output.push(0),
                    b'\\' => output.push(b'\\'),
                    b'a' => output.push(7),
                    b'b' => output.push(8),
                    b'e' => output.push(0x1b),
                    b'n' => output.push(b'\n'),
                    b'r' => output.push(b'\r'),
                    b't' => output.push(b'\t'),
                    b'x' => {
                        if bytes.get(offset) == Some(&b'{') {
                            offset = offset.saturating_add(1);
                            output.push(parse_braced_hex(bytes, &mut offset)?);
                        } else {
                            output.push(parse_hex_byte(bytes, &mut offset)?);
                        }
                    }
                    _ => {
                        return Err(format!(
                            "unsupported quoted escape \\{}",
                            char::from(escaped)
                        ))
                    }
                }
            }
            _ => output.push(byte),
        }
    }
}

fn parse_count(input: &str) -> Result<(usize, &str), String> {
    let input = input.trim_start();
    let end = input
        .find(|value: char| !value.is_ascii_digit())
        .unwrap_or(input.len());
    let digits = input
        .get(..end)
        .ok_or_else(|| "repeat count escaped input".to_string())?;
    if digits.is_empty() {
        return Err("repeat operator requires a count".into());
    }
    let count = digits
        .parse::<usize>()
        .map_err(|_| "invalid repeat count".to_string())?;
    if count > 4_096 {
        return Err("repeat count exceeds 4096".into());
    }
    let rest = input
        .get(end..)
        .ok_or_else(|| "repeat count escaped input".to_string())?;
    Ok((count, rest))
}

fn parse_perl_bytes(input: &str) -> Result<Vec<u8>, String> {
    let mut rest = input.trim();
    let mut output = Vec::new();
    loop {
        let (part, after_string) = parse_quoted(rest)?;
        rest = after_string.trim_start();
        let count;
        if let Some(after_x) = rest.strip_prefix('x') {
            let parsed = parse_count(after_x)?;
            count = parsed.0;
            rest = parsed.1.trim_start();
        } else {
            count = 1;
        }
        let added = part
            .len()
            .checked_mul(count)
            .ok_or_else(|| "expanded string length overflow".to_string())?;
        if output.len().saturating_add(added) > 1_048_576 {
            return Err("expanded string exceeds 1 MiB".into());
        }
        for _ in 0..count {
            output.extend_from_slice(&part);
        }
        if let Some(after_dot) = rest.strip_prefix('.') {
            rest = after_dot.trim_start();
            continue;
        }
        if rest.is_empty() {
            return Ok(output);
        }
        return Err(format!("trailing string expression {rest:?}"));
    }
}

fn file_name(path: &str) -> Result<&str, String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid source path {path:?}"))
}

fn whole_file_exclusion(name: &str) -> Option<&'static str> {
    match name {
        "02parser.test" | "03encoding_utf8.test" => {
            Some("parser callback API is not a td-term product surface")
        }
        "17state_mouse.test" => Some("mouse input is outside the first profile"),
        "25state_input.test" => Some("key encoding belongs to the later input-profile stack"),
        "28state_dbl_wh.test" | "67screen_dbl_wh.test" => {
            Some("double-width and double-height lines are outside the first profile")
        }
        "29state_fallback.test" => Some("fallback callbacks are not a td-term product surface"),
        "32state_flow.test" => Some("host-managed scrollback callbacks are not a td-term surface"),
        "40state_selection.test" => Some("selection is outside the first profile"),
        "62screen_damage.test" => Some("callback damage geometry belongs to renderer parity"),
        "65screen_protect.test" => Some("selective erase protection is outside the first profile"),
        "66screen_extent.test" => Some("attribute extent callbacks are not a td-term surface"),
        "68screen_termprops.test" => Some("screen term-property callbacks await title support"),
        "69screen_reflow.test" => Some("td-term deliberately retains non-reflowing history"),
        _ => None,
    }
}

fn case_profile_exclusion(name: &str, title: &str) -> Option<&'static str> {
    match name {
        "12state_scroll.test"
            if title.contains("DECSLRM") || title.starts_with("DECRQSS") =>
        {
            Some("horizontal margins and status-string queries are outside the first profile")
        }
        "13state_edit.test"
            if title.contains("DECSLRM")
                || title.starts_with("DECIC")
                || title.starts_with("DECDC")
                || matches!(title, "SEL" | "SED" | "DECRQSS on DECSCA") =>
        {
            Some("horizontal margins and selective or column edits are outside the first profile")
        }
        "14state_encoding.test"
            if !matches!(
                title,
                "Default"
                    | "Designate G0=DEC drawing"
                    | "Designate G1 + LS1"
                    | "LS0"
                    | "Mixed US-ASCII and UTF-8"
            ) =>
        {
            Some("the first profile has only G0/G1 ASCII and DEC graphics")
        }
        "15state_mode.test"
            if !matches!(
                title,
                "DEC origin mode"
                    | "Origin mode bounds cursor to scrolling region"
                    | "Origin mode without scroll region"
            ) =>
        {
            Some(
                "insert, newline, query, and horizontal-margin modes are outside the first profile",
            )
        }
        "18state_termprops.test" if title != "Cursor visibility" => {
            Some("cursor blink, cursor shape, and title properties are outside the first profile")
        }
        "26state_query.test" if !matches!(title, "DA" | "DSR" | "CPR") => {
            Some("only primary DA and standard status and cursor reports are in the first profile")
        }
        "64screen_pen.test"
            if matches!(
                title,
                "Font"
                    | "Super/subscript"
                    | "DECSCNM xors reverse for entire screen"
                    | "Set default colours"
            ) =>
        {
            Some(
                "font, script, reverse-screen, and default-palette controls are outside the first profile",
            )
        }
        "90vttest_01-movement-1.test" => {
            Some("DEC screen alignment is outside the first profile")
        }
        _ => None,
    }
}

fn excluded_case_replay(name: &str, title: &str) -> Option<&'static [u8]> {
    match (name, title) {
        ("64screen_pen.test", "Font") => Some(b"F"),
        ("64screen_pen.test", "Super/subscript") => Some(b"x02"),
        _ => None,
    }
}

fn csi_parameter(payload: &[u8], value: &[u8]) -> bool {
    payload
        .split(|byte| *byte == b';' || *byte == b':')
        .any(|parameter| parameter == value)
}

fn decimal_parameter(field: &[u8]) -> Option<u16> {
    let digits = field.split(|byte| *byte == b':').next()?;
    if digits.is_empty() {
        return Some(0);
    }
    digits.iter().try_fold(0u16, |value, digit| {
        let decimal = digit.checked_sub(b'0')?;
        if decimal > 9 {
            return None;
        }
        value.checked_mul(10)?.checked_add(u16::from(decimal))
    })
}

fn excluded_sgr(payload: &[u8]) -> Option<&'static str> {
    let mut fields = payload.split(|byte| *byte == b';');
    while let Some(field) = fields.next() {
        match decimal_parameter(field) {
            Some(38 | 48) if field.contains(&b':') => return Some("colon-separated color"),
            Some(38 | 48) => match fields.next().and_then(decimal_parameter) {
                Some(5) => {
                    let _ = fields.next();
                }
                Some(2) => {
                    let first_component = fields.next();
                    if first_component.is_some_and(<[u8]>::is_empty) {
                        let _ = fields.next();
                    }
                    let _ = fields.next();
                    let _ = fields.next();
                }
                _ => {}
            },
            Some(11) => return Some("alternate font"),
            Some(73) => return Some("superscript"),
            Some(74) => return Some("subscript"),
            _ => {}
        }
    }
    None
}

fn excluded_csi(payload: &[u8], final_byte: u8) -> Option<&'static str> {
    let private = payload.first() == Some(&b'?');
    let parameters = if private {
        payload.get(1..).unwrap_or_default()
    } else {
        payload
    };
    match final_byte {
        b'J' if private => Some("selective display erase"),
        b'K' if private => Some("selective line erase"),
        b'h' | b'l' if private && csi_parameter(parameters, b"69") => {
            Some("horizontal-margin mode")
        }
        b'h' | b'l' if private && csi_parameter(parameters, b"12") => Some("cursor-blink mode"),
        b'h' | b'l' if private && csi_parameter(parameters, b"5") => Some("reverse-screen mode"),
        b'h' | b'l' if !private && csi_parameter(parameters, b"4") => Some("insert mode"),
        b'h' | b'l' if !private && csi_parameter(parameters, b"20") => Some("newline mode"),
        b's' if !payload.is_empty() => Some("horizontal margins"),
        b'p' if payload.last() == Some(&b'$') => Some("mode query"),
        b'q' if payload.first() == Some(&b'>') => Some("terminal version query"),
        b'q' if payload.last() == Some(&b' ') => Some("cursor shape"),
        b'q' if payload.last() == Some(&b'"') => Some("protected cells"),
        b'n' if private => Some("private cursor report"),
        b'}' if payload.last() == Some(&b'\'') => Some("insert columns"),
        b'~' if payload.last() == Some(&b'\'') => Some("delete columns"),
        b'm' => excluded_sgr(parameters),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ProtocolScan {
    Ground,
    Escape,
    EscapeHash,
    EscapeDesignate,
    EscapeSpace,
    Csi,
    Dcs { dollar: bool },
}

struct ProtocolResult {
    excluded: Option<&'static str>,
    settled: bool,
}

impl ProtocolResult {
    fn excluded(reason: &'static str) -> Self {
        Self {
            excluded: Some(reason),
            settled: false,
        }
    }
}

fn scan_protocol(bytes: &[u8]) -> ProtocolResult {
    let mut state = ProtocolScan::Ground;
    let mut csi = Vec::new();
    let mut utf8_remaining = 0u8;
    for byte in bytes {
        if matches!(state, ProtocolScan::Ground) {
            if utf8_remaining != 0 {
                if byte & 0xc0 == 0x80 {
                    utf8_remaining = utf8_remaining.saturating_sub(1);
                    continue;
                }
                if matches!(*byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f) {
                    continue;
                }
            }
            utf8_remaining = match *byte {
                0xc2..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf4 => 3,
                _ => 0,
            };
            if utf8_remaining != 0 {
                continue;
            }
        }
        state = match state {
            ProtocolScan::Ground => match *byte {
                0x1b => ProtocolScan::Escape,
                0x8e => return ProtocolResult::excluded("single shift 2"),
                0x8f => return ProtocolResult::excluded("single shift 3"),
                0x90 => return ProtocolResult::excluded("8-bit DCS"),
                0x9b => return ProtocolResult::excluded("8-bit CSI"),
                0x9d => return ProtocolResult::excluded("terminal title"),
                _ => ProtocolScan::Ground,
            },
            ProtocolScan::Escape => match *byte {
                b'[' => {
                    csi.clear();
                    ProtocolScan::Csi
                }
                b']' => return ProtocolResult::excluded("terminal title"),
                b'P' => ProtocolScan::Dcs { dollar: false },
                b'#' => ProtocolScan::EscapeHash,
                b'(' | b')' => ProtocolScan::EscapeDesignate,
                b' ' => ProtocolScan::EscapeSpace,
                b'*' => return ProtocolResult::excluded("G2 character set"),
                b'+' => return ProtocolResult::excluded("G3 character set"),
                b'n' => return ProtocolResult::excluded("G2 locking shift"),
                b'o' => return ProtocolResult::excluded("G3 locking shift"),
                b'~' => return ProtocolResult::excluded("right-side G1 locking shift"),
                b'}' => return ProtocolResult::excluded("right-side G2 locking shift"),
                b'|' => return ProtocolResult::excluded("right-side G3 locking shift"),
                0x18 | 0x1a => ProtocolScan::Ground,
                0x1b => ProtocolScan::Escape,
                0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f => ProtocolScan::Escape,
                _ => ProtocolScan::Ground,
            },
            ProtocolScan::EscapeHash => {
                if matches!(*byte, 0x18 | 0x1a) {
                    ProtocolScan::Ground
                } else if *byte == 0x1b {
                    ProtocolScan::Escape
                } else if matches!(*byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f) {
                    ProtocolScan::EscapeHash
                } else if matches!(*byte, b'3' | b'4' | b'5' | b'6') {
                    return ProtocolResult::excluded("double-width or double-height line");
                } else if *byte == b'8' {
                    return ProtocolResult::excluded("DEC screen alignment");
                } else {
                    ProtocolScan::Ground
                }
            }
            ProtocolScan::EscapeDesignate => {
                if matches!(*byte, 0x18 | 0x1a) {
                    ProtocolScan::Ground
                } else if *byte == 0x1b {
                    ProtocolScan::Escape
                } else if matches!(*byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f) {
                    ProtocolScan::EscapeDesignate
                } else if *byte != b'B' && *byte != b'0' {
                    return ProtocolResult::excluded("unsupported G0/G1 character set");
                } else {
                    ProtocolScan::Ground
                }
            }
            ProtocolScan::EscapeSpace => {
                if matches!(*byte, 0x18 | 0x1a) {
                    ProtocolScan::Ground
                } else if *byte == 0x1b {
                    ProtocolScan::Escape
                } else if matches!(*byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f) {
                    ProtocolScan::EscapeSpace
                } else if *byte == b'G' {
                    return ProtocolResult::excluded("8-bit reply mode");
                } else if *byte == b'F' {
                    return ProtocolResult::excluded("7-bit reply-mode reset");
                } else {
                    ProtocolScan::Ground
                }
            }
            ProtocolScan::Csi => {
                if (0x40..=0x7e).contains(byte) {
                    if let Some(reason) = excluded_csi(&csi, *byte) {
                        return ProtocolResult::excluded(reason);
                    }
                    csi.clear();
                    ProtocolScan::Ground
                } else if (0x20..=0x3f).contains(byte) {
                    csi.push(*byte);
                    ProtocolScan::Csi
                } else if matches!(*byte, 0x18 | 0x1a) {
                    csi.clear();
                    ProtocolScan::Ground
                } else if *byte == 0x1b {
                    csi.clear();
                    ProtocolScan::Escape
                } else {
                    ProtocolScan::Csi
                }
            }
            ProtocolScan::Dcs { dollar } => {
                if dollar && *byte == b'q' {
                    return ProtocolResult::excluded("status-string query");
                }
                match *byte {
                    0x18 | 0x1a => ProtocolScan::Ground,
                    0x1b => ProtocolScan::Escape,
                    0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f => ProtocolScan::Dcs { dollar },
                    b'$' => ProtocolScan::Dcs { dollar: true },
                    _ => ProtocolScan::Dcs { dollar: false },
                }
            }
        };
    }
    ProtocolResult {
        excluded: None,
        settled: matches!(state, ProtocolScan::Ground) && utf8_remaining == 0,
    }
}

fn excluded_protocol(bytes: &[u8]) -> Option<&'static str> {
    scan_protocol(bytes).excluded
}

fn csi_ends_at_operation_boundary(bytes: &[u8], body_start: usize) -> bool {
    for (index, byte) in bytes.iter().enumerate().skip(body_start) {
        if (0x40..=0x7e).contains(byte) {
            return index.saturating_add(1) == bytes.len();
        }
        if !(0x20..=0x3f).contains(byte) {
            return false;
        }
    }
    false
}

fn escape_ends_at_operation_boundary(bytes: &[u8]) -> bool {
    for (index, byte) in bytes.iter().enumerate().skip(1) {
        if (0x30..=0x7e).contains(byte) {
            return index.saturating_add(1) == bytes.len();
        }
        if !(0x20..=0x2f).contains(byte) {
            return false;
        }
    }
    false
}

fn string_ends_at_operation_boundary(bytes: &[u8], body_start: usize, osc: bool) -> bool {
    let mut escape = false;
    for (index, byte) in bytes.iter().enumerate().skip(body_start) {
        let at_end = index.saturating_add(1) == bytes.len();
        if (osc && *byte == 0x07) || *byte == 0x9c {
            return at_end;
        }
        if escape {
            return *byte == b'\\' && at_end;
        }
        if *byte == 0x1b {
            escape = true;
        }
    }
    false
}

fn standalone_prunable_query(bytes: &[u8]) -> bool {
    let Some(reason) = excluded_protocol(bytes) else {
        return false;
    };
    if !matches!(
        reason,
        "mode query" | "status-string query" | "terminal version query" | "private cursor report"
    ) {
        return false;
    }
    match (bytes.first(), bytes.get(1)) {
        (Some(0x1b), Some(b'[')) => csi_ends_at_operation_boundary(bytes, 2),
        (Some(0x1b), Some(b'P' | b'_' | b'^')) => {
            string_ends_at_operation_boundary(bytes, 2, false)
        }
        (Some(0x1b), Some(b']')) => string_ends_at_operation_boundary(bytes, 2, true),
        (Some(0x1b), Some(_)) => escape_ends_at_operation_boundary(bytes),
        (Some(0x8e | 0x8f), None) => true,
        (Some(0x90), _) => string_ends_at_operation_boundary(bytes, 1, false),
        (Some(0x9b), _) => csi_ends_at_operation_boundary(bytes, 1),
        (Some(0x9d), _) => string_ends_at_operation_boundary(bytes, 1, true),
        _ => false,
    }
}

fn tags_for(name: &str) -> Vec<&'static str> {
    let mut tags = vec!["core"];
    match name {
        "10state_putglyph.test" | "61screen_unicode.test" => tags.push("utf8"),
        "13state_edit.test" | "31state_rep.test" => tags.push("editing"),
        "14state_encoding.test" => tags.push("charset"),
        "15state_mode.test"
        | "18state_termprops.test"
        | "22state_save.test"
        | "27state_reset.test" => tags.push("modes"),
        "16state_resize.test" | "63screen_resize.test" => tags.push("resize"),
        "20state_wrapping.test" => tags.push("wrapping"),
        "26state_query.test" => tags.push("replies"),
        "30state_pen.test" | "64screen_pen.test" => tags.push("color"),
        "60screen_ascii.test" => tags.push("alternate-screen"),
        _ => {}
    }
    tags
}

fn slug(input: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for scalar in input.chars() {
        if scalar.is_ascii_alphanumeric() {
            output.push(scalar.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
        if output.len() >= 72 {
            break;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "case".into()
    } else {
        output
    }
}

fn source_identity(path: &str, case: &SourceCase) -> String {
    format!("{RELEASE}:{path}:{}:{}", case.number, case.title)
}

fn native_escape_bytes(bytes: &[u8]) -> String {
    let mut output = String::from("b\"");
    for byte in bytes {
        match byte {
            0 => output.push_str("\\0"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x1b => output.push_str("\\e"),
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            b' '..=b'~' => output.push(char::from(*byte)),
            _ => {
                let _ = write!(output, "\\x{byte:02x}");
            }
        }
    }
    output.push('"');
    output
}

fn native_escape_text(input: &str) -> String {
    let mut output = String::from("\"");
    for scalar in input.chars() {
        match scalar {
            '\0' => output.push_str("\\0"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{1b}' => output.push_str("\\e"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            value if value.is_control() => {
                let code = u32::from(value);
                if code <= u32::from(u8::MAX) {
                    let _ = write!(output, "\\x{code:02x}");
                }
            }
            _ => output.push(scalar),
        }
    }
    output.push('"');
    output
}

fn parse_pair(input: &str, separator: char, what: &str) -> Result<(usize, usize), String> {
    let Some((first, second)) = input.trim().split_once(separator) else {
        return Err(format!("{what} requires two coordinates"));
    };
    let first = first
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid {what} first coordinate"))?;
    let second = second
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid {what} second coordinate"))?;
    Ok((first, second))
}

fn parse_resize(input: &str) -> Result<(usize, usize), String> {
    let (rows, columns) = parse_pair(input, ',', "resize")?;
    if rows == 0 || columns == 0 || rows > 4_096 || columns > 4_096 {
        return Err(format!("resize escapes bounds: {rows}x{columns}"));
    }
    rows.checked_mul(columns)
        .filter(|count| *count <= 16_777_216)
        .ok_or_else(|| format!("resize grid is too large: {rows}x{columns}"))?;
    Ok((rows, columns))
}

fn parse_codepoint(input: &str) -> Result<char, String> {
    let digits = input
        .trim()
        .strip_prefix("0x")
        .ok_or_else(|| format!("codepoint is not hexadecimal: {input:?}"))?;
    let value =
        u32::from_str_radix(digits, 16).map_err(|_| format!("invalid codepoint {input:?}"))?;
    char::from_u32(value).ok_or_else(|| format!("invalid Unicode scalar {input:?}"))
}

fn convert_putglyph(text: &str) -> Result<Option<String>, String> {
    let rest = text
        .strip_prefix("putglyph ")
        .ok_or_else(|| "putglyph has no arguments".to_string())?;
    let mut words = rest.split_whitespace();
    let scalars = words
        .next()
        .ok_or_else(|| "putglyph has no scalar".to_string())?;
    let width = words
        .next()
        .ok_or_else(|| "putglyph has no width".to_string())?
        .parse::<usize>()
        .map_err(|_| "putglyph has invalid width".to_string())?;
    let position = words
        .next()
        .ok_or_else(|| "putglyph has no position".to_string())?;
    if words.next().is_some() || width != 1 || scalars.contains(',') {
        return Ok(None);
    }
    let scalar = parse_codepoint(scalars)?;
    let (row, column) = parse_pair(position, ',', "putglyph position")?;
    Ok(Some(format!(
        "expect glyph {row} {column} {}",
        native_escape_text(&scalar.to_string())
    )))
}

fn convert_cursor(text: &str, columns: usize) -> Result<Option<String>, String> {
    let position = if let Some(rest) = text.strip_prefix("?cursor") {
        rest.trim()
            .strip_prefix('=')
            .ok_or_else(|| "cursor assertion has no equals sign".to_string())?
            .trim()
    } else if let Some(rest) = text.strip_prefix("cursor ") {
        rest.trim()
    } else {
        return Err("cursor assertion has invalid prefix".into());
    };
    let (row, column) = parse_pair(position, ',', "cursor")?;
    // libvterm's cursor callback cannot distinguish a real right-edge cursor
    // from its pending-wrap phantom.
    if column >= columns.saturating_sub(1) {
        return Ok(None);
    }
    Ok(Some(format!("expect cursor {row} {column}")))
}

fn parse_screen_row(text: &str, columns: usize) -> Result<Option<String>, String> {
    let rest = text
        .strip_prefix("?screen_row ")
        .ok_or_else(|| "screen_row has invalid prefix".to_string())?;
    let Some((row, expected)) = rest.split_once('=') else {
        return Err("screen_row has no equals sign".into());
    };
    let row = row
        .trim()
        .parse::<usize>()
        .map_err(|_| "screen_row has invalid row".to_string())?;
    let expected = expected.trim();
    if !expected.starts_with('"') {
        return Ok(None);
    }
    let bytes = parse_perl_bytes(expected)?;
    if !bytes.is_ascii() {
        return Ok(None);
    }
    let mut value =
        String::from_utf8(bytes).map_err(|_| "screen_row is not valid UTF-8".to_string())?;
    let width = value.chars().count();
    if width > columns {
        return Ok(None);
    }
    value.extend(std::iter::repeat_n(' ', columns.saturating_sub(width)));
    Ok(Some(format!(
        "expect text {row} {}",
        native_escape_text(&value)
    )))
}

fn parse_rgb(input: &str) -> Result<(u8, u8, u8), String> {
    let body = input
        .strip_prefix("rgb(")
        .and_then(|value| value.strip_suffix(')'))
        .ok_or_else(|| format!("invalid RGB color {input:?}"))?;
    let values = body
        .split(',')
        .map(|value| {
            value
                .parse::<u8>()
                .map_err(|_| format!("invalid RGB component {value:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let red = values
        .first()
        .copied()
        .ok_or_else(|| "RGB color has no red component".to_string())?;
    let green = values
        .get(1)
        .copied()
        .ok_or_else(|| "RGB color has no green component".to_string())?;
    let blue = values
        .get(2)
        .copied()
        .ok_or_else(|| "RGB color has no blue component".to_string())?;
    if values.len() != 3 {
        return Err("RGB color has extra components".into());
    }
    Ok((red, green, blue))
}

fn native_color(input: &str, foreground: bool) -> Result<String, String> {
    let rgb = parse_rgb(input)?;
    if (foreground && rgb == (240, 240, 240)) || (!foreground && rgb == (0, 0, 0)) {
        return Ok("default".into());
    }
    let palette = [
        ((0, 0, 0), 0u8),
        ((224, 0, 0), 1),
        ((0, 224, 0), 2),
        ((224, 224, 0), 3),
        ((0, 0, 224), 4),
        ((224, 0, 224), 5),
        ((0, 224, 224), 6),
        ((224, 224, 224), 7),
    ];
    if let Some((_, index)) = palette.iter().find(|(color, _)| *color == rgb) {
        return Ok(format!("indexed:{index}"));
    }
    Ok(format!("rgb:{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2))
}

fn native_attributes(input: &str) -> Result<Option<String>, String> {
    let mut rest = input;
    let mut names = Vec::new();
    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix('B') {
            names.push("bold");
            rest = next;
        } else if let Some(next) = rest.strip_prefix('I') {
            names.push("italic");
            rest = next;
        } else if let Some(next) = rest.strip_prefix('R') {
            names.push("inverse");
            rest = next;
        } else if let Some(next) = rest.strip_prefix("U1") {
            names.push("underline");
            rest = next;
        } else {
            return Ok(None);
        }
    }
    if names.is_empty() {
        Ok(Some("none".into()))
    } else {
        Ok(Some(names.join(",")))
    }
}

fn token_value<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    input
        .split_whitespace()
        .find_map(|word| word.strip_prefix(prefix))
}

fn convert_screen_cell(text: &str) -> Result<Option<String>, String> {
    let rest = text
        .strip_prefix("?screen_cell ")
        .ok_or_else(|| "screen_cell has invalid prefix".to_string())?;
    let Some((position, expected)) = rest.split_once('=') else {
        return Err("screen_cell has no equals sign".into());
    };
    let (row, column) = parse_pair(position, ',', "screen_cell position")?;
    let expected = expected.trim();
    let close = expected
        .find('}')
        .ok_or_else(|| "screen_cell has unterminated scalar list".to_string())?;
    let scalars = expected
        .strip_prefix('{')
        .and_then(|value| value.get(..close.saturating_sub(1)))
        .ok_or_else(|| "screen_cell has invalid scalar list".to_string())?;
    let tail = expected
        .get(close.saturating_add(1)..)
        .ok_or_else(|| "screen_cell tail escaped input".to_string())?
        .trim();
    let scalar = if scalars.trim().is_empty() {
        ' '
    } else {
        if scalars.contains(',') {
            return Ok(None);
        }
        parse_codepoint(scalars.trim())?
    };
    let width = token_value(tail, "width=")
        .ok_or_else(|| "screen_cell has no width".to_string())?
        .parse::<usize>()
        .map_err(|_| "screen_cell has invalid width".to_string())?;
    if width != 1
        || tail
            .split_whitespace()
            .any(|word| word.starts_with("dwl") || word.starts_with("dhl-"))
    {
        return Ok(None);
    }
    let attrs_start = tail
        .find("attrs={")
        .ok_or_else(|| "screen_cell has no attrs".to_string())?
        .saturating_add("attrs={".len());
    let attrs_tail = tail
        .get(attrs_start..)
        .ok_or_else(|| "screen_cell attrs escaped input".to_string())?;
    let attrs_end = attrs_tail
        .find('}')
        .ok_or_else(|| "screen_cell has unterminated attrs".to_string())?;
    let attrs = attrs_tail
        .get(..attrs_end)
        .ok_or_else(|| "screen_cell attrs escaped input".to_string())?;
    let Some(attributes) = native_attributes(attrs)? else {
        return Ok(None);
    };
    let foreground = token_value(tail, "fg=")
        .ok_or_else(|| "screen_cell has no foreground".to_string())
        .and_then(|value| native_color(value, true))?;
    let background = token_value(tail, "bg=")
        .ok_or_else(|| "screen_cell has no background".to_string())
        .and_then(|value| native_color(value, false))?;
    Ok(Some(format!(
        "expect cell {row} {column} {} fg={foreground} bg={background} attrs={attributes}",
        native_escape_text(&scalar.to_string())
    )))
}

fn convert_settermprop(text: &str) -> Result<Option<String>, String> {
    let rest = text
        .strip_prefix("settermprop ")
        .ok_or_else(|| "settermprop has invalid prefix".to_string())?;
    let mut words = rest.split_whitespace();
    let property = words
        .next()
        .ok_or_else(|| "settermprop has no property".to_string())?;
    let value = words
        .next()
        .ok_or_else(|| "settermprop has no value".to_string())?;
    if words.next().is_some() {
        return Ok(None);
    }
    let state = match value {
        "true" => "on",
        "false" => "off",
        _ => return Ok(None),
    };
    let mode = match property {
        "1" => "cursor-visible",
        "3" => "alternate-screen",
        _ => return Ok(None),
    };
    Ok(Some(format!("expect mode {mode} {state}")))
}

#[derive(Clone, Copy)]
enum OutputPolicy {
    Source,
    PrimaryDeviceAttributes,
    Unsupported,
}

fn output_policy(input: Option<&[u8]>) -> OutputPolicy {
    let Some(input) = input else {
        return OutputPolicy::Source;
    };
    if standalone_prunable_query(input) {
        return OutputPolicy::Unsupported;
    }
    if matches!(input, b"\x1b[c" | b"\x1b[0c") {
        return OutputPolicy::PrimaryDeviceAttributes;
    }
    if input.windows(2).any(|window| window == b"$p")
        || input.windows(4).any(|window| window == b"\x1bP$q")
        || matches!(input, b"\x1b[>q" | b"\x1b[?6n")
    {
        return OutputPolicy::Unsupported;
    }
    OutputPolicy::Source
}

fn convert_output(
    text: &str,
    replies: &mut Vec<u8>,
    policy: OutputPolicy,
) -> Result<String, String> {
    let expression = text
        .strip_prefix("output ")
        .ok_or_else(|| "output has invalid prefix".to_string())?;
    let source = parse_perl_bytes(expression)?;
    match policy {
        OutputPolicy::Source => replies.extend_from_slice(&source),
        OutputPolicy::PrimaryDeviceAttributes => {
            if source != b"\x1b[?1;2c" {
                return Err("primary device attributes source identity changed".into());
            }
            replies.extend_from_slice(b"\x1b[?1;0c");
        }
        OutputPolicy::Unsupported => {
            return Err("unsupported output policy reached conversion".into())
        }
    }
    Ok(format!("expect reply {}", native_escape_bytes(replies)))
}

fn assertion_kind(text: &str) -> Result<&str, String> {
    assertion_name(text).ok_or_else(|| "assertion has no kind".to_string())
}

fn translate_assertion(
    text: &str,
    columns: usize,
    replies: &mut Vec<u8>,
    output_policy: OutputPolicy,
) -> Result<Option<String>, String> {
    match assertion_kind(text)? {
        "putglyph" => convert_putglyph(text),
        "cursor" => convert_cursor(text, columns),
        "screen_row" => parse_screen_row(text, columns),
        "screen_cell" => convert_screen_cell(text),
        "settermprop" => convert_settermprop(text),
        "output" => match output_policy {
            OutputPolicy::Unsupported => Ok(None),
            policy => convert_output(text, replies, policy).map(Some),
        },
        _ => Ok(None),
    }
}

fn screen_cell_crosses_profile(text: &str) -> bool {
    let Some((_, expected)) = text.split_once('=') else {
        return false;
    };
    let Some(scalars) = expected
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(scalars, _)| scalars)
    else {
        return false;
    };
    let width = token_value(expected, "width=").and_then(|value| value.parse::<usize>().ok());
    scalars.contains(',')
        || width.is_some_and(|value| value != 1)
        || expected
            .split_whitespace()
            .any(|word| word.starts_with("dwl") || word.starts_with("dhl-"))
}

fn unsupported_observation_reason(kind: &str, text: &str) -> Option<&'static str> {
    match kind {
        "sb_popline" => Some("host-managed scrollback callback injects content"),
        "putglyph" => {
            Some("wide, combining, or protected glyph semantics are outside the first profile")
        }
        "screen_cell" if screen_cell_crosses_profile(text) => {
            Some("wide or combining cell semantics are outside the first profile")
        }
        _ => None,
    }
}

fn ignored_configuration(command: &str) -> bool {
    matches!(
        command,
        "DAMAGEFLUSH"
            | "DAMAGEMERGE"
            | "INIT"
            | "UTF8"
            | "WANTENCODING"
            | "WANTPARSER"
            | "WANTSCREEN"
            | "WANTSTATE"
    )
}

fn unsupported_command_reason(command: &str) -> &'static str {
    match command {
        "ENCIN" | "INCHAR" | "INKEY" => "input encoding operation is outside this stack",
        "FOCUS" => "focus reporting input is outside this stack",
        "MOUSEBTN" | "MOUSEMOVE" => "mouse input is outside the first profile",
        "PASTE" => "bracketed paste input is outside this stack",
        "SELECTION" => "selection operation is outside the first profile",
        "SETDEFAULTCOL" => "custom default palette is outside the first profile",
        _ => "unsupported source operation",
    }
}

fn count_exclusion(map: &mut BTreeMap<String, usize>, reason: impl Into<String>) {
    let reason = reason.into();
    let count = map.entry(reason).or_default();
    *count = count.saturating_add(1);
}

fn operation_step(operation: &Operation) -> String {
    match operation {
        Operation::Write(bytes) => format!("write {}", native_escape_bytes(bytes)),
        Operation::Resize(rows, columns) => format!("resize {rows} {columns}"),
    }
}

fn replay_steps(operations: &[ReplayOperation]) -> Vec<NativeStep> {
    let mut steps = Vec::new();
    for replay in operations {
        steps.push(NativeStep::Operation(replay.operation.clone()));
        if let Some(replies) = &replay.reply_after {
            steps.push(NativeStep::Expect(format!(
                "expect reply {}",
                native_escape_bytes(replies)
            )));
        }
    }
    steps
}

fn render_case(case: &NativeCase, output: &mut String) {
    let _ = writeln!(output, "case {}", case.id);
    let _ = writeln!(output, "source {}", native_escape_text(&case.source));
    let _ = writeln!(output, "tags {}", case.tags.join(" "));
    let _ = writeln!(output, "size {} {}", case.rows, case.columns);
    for step in &case.steps {
        match step {
            NativeStep::Operation(operation) => {
                let _ = writeln!(output, "{}", operation_step(operation));
            }
            NativeStep::Expect(expectation) => {
                let _ = writeln!(output, "{expectation}");
            }
        }
    }
    output.push_str("end\n\n");
}

fn validate_native_profile(cases: &[NativeCase]) -> Result<(), String> {
    for case in cases {
        if let Some(reason) = excluded_protocol_steps(&case.steps) {
            return Err(format!(
                "{} retains out-of-profile operation: {reason}",
                case.id
            ));
        }
    }
    Ok(())
}

fn native_write_stream(steps: &[NativeStep]) -> Vec<u8> {
    let mut stream = Vec::new();
    for step in steps {
        let NativeStep::Operation(Operation::Write(bytes)) = step else {
            continue;
        };
        stream.extend_from_slice(bytes);
    }
    stream
}

fn excluded_protocol_steps(steps: &[NativeStep]) -> Option<&'static str> {
    excluded_protocol(&native_write_stream(steps))
}

fn unique_id(path: &str, title: &str, ids: &mut BTreeSet<String>) -> Result<String, String> {
    let name = file_name(path)?
        .strip_suffix(".test")
        .ok_or_else(|| format!("source file lacks .test suffix: {path}"))?
        .replace('_', "-");
    let base = format!("libvterm/{name}/{}", slug(title));
    if ids.insert(base.clone()) {
        return Ok(base);
    }
    for suffix in 2..=1_024usize {
        let candidate = format!("{base}-{suffix}");
        if ids.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(format!("too many duplicate case titles for {path}:{title}"))
}

fn execute_state_lines(
    source: (&str, &[SourceLine], bool),
    operations: &mut Vec<ReplayOperation>,
    replies: &mut Vec<u8>,
    baseline_rows: &mut usize,
    baseline_columns: &mut usize,
    rows: &mut usize,
    columns: &mut usize,
) -> Result<(), String> {
    let (path, lines, replay_writes) = source;
    for line in lines {
        if is_assertion(&line.text) {
            if assertion_kind(&line.text)? == "output" {
                let _ = convert_output(&line.text, replies, OutputPolicy::Source)?;
                let replay = operations
                    .iter_mut()
                    .rev()
                    .find(|replay| replay.reply_after.is_some())
                    .ok_or_else(|| {
                        format!(
                            "{}:{}: output has no reply-capable operation",
                            path, line.number
                        )
                    })?;
                replay.reply_after = Some(replies.clone());
            }
            continue;
        }
        let command = command_name(&line.text)
            .ok_or_else(|| format!("{}:{}: command has no name", path, line.number))?;
        let rest = line.text.strip_prefix(command).unwrap_or("").trim();
        match command {
            "RESET" => {
                operations.clear();
                replies.clear();
                *baseline_rows = *rows;
                *baseline_columns = *columns;
            }
            "PUSH" => {
                if !replay_writes {
                    continue;
                }
                let bytes = parse_perl_bytes(rest)
                    .map_err(|error| format!("{}:{}: {error}", path, line.number))?;
                operations.push(ReplayOperation {
                    operation: Operation::Write(bytes),
                    reply_after: Some(replies.clone()),
                });
            }
            "RESIZE" => {
                let size = parse_resize(rest)
                    .map_err(|error| format!("{}:{}: {error}", path, line.number))?;
                *rows = size.0;
                *columns = size.1;
                operations.push(ReplayOperation {
                    operation: Operation::Resize(size.0, size.1),
                    reply_after: None,
                });
            }
            _ if ignored_configuration(command) => {}
            _ => {}
        }
    }
    Ok(())
}

fn record_case_exclusion(counts: &mut Counts, source: String, reason: &str) {
    count_exclusion(&mut counts.excluded_cases, reason);
    counts
        .excluded_case_records
        .push((source, reason.to_string()));
}

fn count_case_assertions(case: &SourceCase, counts: &mut Counts) {
    for line in &case.lines {
        if is_assertion(&line.text) {
            if let Some(kind) = assertion_name(&line.text) {
                count_exclusion(&mut counts.excluded_assertions, kind);
            }
        }
    }
}

#[derive(Clone)]
struct PendingExpectation {
    text: String,
    kind: Option<String>,
}

fn expectation_key(expectation: &str) -> Result<String, String> {
    let mut words = expectation.split_whitespace();
    if words.next() != Some("expect") {
        return Err(format!("invalid generated expectation {expectation:?}"));
    }
    let kind = words
        .next()
        .ok_or_else(|| format!("generated expectation has no kind: {expectation:?}"))?;
    match kind {
        "cell" | "glyph" => {
            let row = words
                .next()
                .ok_or_else(|| format!("generated {kind} has no row: {expectation:?}"))?;
            let column = words
                .next()
                .ok_or_else(|| format!("generated {kind} has no column: {expectation:?}"))?;
            Ok(format!("cell:{row}:{column}"))
        }
        "cursor" | "reply" => Ok(kind.to_string()),
        "mode" | "row" | "text" => {
            let identity = words
                .next()
                .ok_or_else(|| format!("generated {kind} has no identity: {expectation:?}"))?;
            let keyed_kind = if kind == "text" { "row" } else { kind };
            Ok(format!("{keyed_kind}:{identity}"))
        }
        _ => Err(format!("unsupported generated expectation kind {kind:?}")),
    }
}

fn queue_expectation(
    pending: &mut BTreeMap<String, PendingExpectation>,
    text: String,
    kind: Option<&str>,
    unconverted: &mut Vec<String>,
) -> Result<(), String> {
    let key = expectation_key(&text)?;
    if let Some(previous) = pending.insert(
        key,
        PendingExpectation {
            text,
            kind: kind.map(str::to_string),
        },
    ) {
        if let Some(kind) = previous.kind {
            unconverted.push(kind);
        }
    }
    Ok(())
}

fn flush_expectations(
    pending: &mut BTreeMap<String, PendingExpectation>,
    steps: &mut Vec<NativeStep>,
    converted: &mut usize,
    retained_kinds: &mut Vec<String>,
) {
    for (_, expectation) in std::mem::take(pending) {
        steps.push(NativeStep::Expect(expectation.text));
        if let Some(kind) = expectation.kind {
            *converted = converted.saturating_add(1);
            retained_kinds.push(kind);
        }
    }
}

fn convert_file(
    file: &ParsedFile,
    ids: &mut BTreeSet<String>,
    native: &mut Vec<NativeCase>,
    counts: &mut Counts,
) -> Result<(), String> {
    let name = file_name(&file.path)?;
    if let Some(reason) = whole_file_exclusion(name) {
        for case in &file.cases {
            let source = source_identity(&file.path, case);
            record_case_exclusion(counts, source, reason);
            count_case_assertions(case, counts);
        }
        for line in &file.setup {
            if is_assertion(&line.text) {
                if let Some(kind) = assertion_name(&line.text) {
                    count_exclusion(&mut counts.excluded_assertions, kind);
                }
            }
        }
        return Ok(());
    }

    let mut operations = Vec::new();
    let mut replies = Vec::new();
    let mut rows = 25usize;
    let mut columns = 80usize;
    let mut baseline_rows = rows;
    let mut baseline_columns = columns;
    execute_state_lines(
        (&file.path, &file.setup, true),
        &mut operations,
        &mut replies,
        &mut baseline_rows,
        &mut baseline_columns,
        &mut rows,
        &mut columns,
    )?;
    for line in &file.setup {
        if is_assertion(&line.text) {
            if let Some(kind) = assertion_name(&line.text) {
                count_exclusion(&mut counts.excluded_assertions, kind);
            }
        }
    }

    for case in &file.cases {
        let mut operation_snapshot = operations.clone();
        let mut reply_snapshot = replies.clone();
        let mut row_snapshot = rows;
        let mut column_snapshot = columns;
        let mut baseline_row_snapshot = baseline_rows;
        let mut baseline_column_snapshot = baseline_columns;
        let profile_exclusion = case_profile_exclusion(name, &case.title);
        let source = source_identity(&file.path, case);
        let mut case_rows = baseline_rows;
        let mut case_columns = baseline_columns;
        let mut steps = replay_steps(&operations);
        let mut profile_stream = native_write_stream(&steps);
        let mut case_replies = replies.clone();
        let mut converted = 0usize;
        let mut unsupported = profile_exclusion;
        let mut unsupported_property = false;
        let mut output_assertion = false;
        let mut last_write = None;
        let mut assertion_kinds = Vec::new();
        let mut retained_converted_kinds = Vec::new();
        let mut unconverted_kinds = Vec::new();
        let mut pending = BTreeMap::new();
        let observed_end = case
            .lines
            .iter()
            .rposition(|line| is_assertion(&line.text))
            .map(|index| index.saturating_add(1))
            .unwrap_or(case.lines.len());
        let (observed_lines, postlude) = case.lines.split_at(observed_end);
        for line in observed_lines {
            if is_assertion(&line.text) {
                let kind = assertion_kind(&line.text)
                    .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                assertion_kinds.push(kind.to_string());
                output_assertion |= kind == "output";
                let policy = if kind == "output" {
                    output_policy(last_write.as_deref())
                } else {
                    OutputPolicy::Source
                };
                let translated =
                    translate_assertion(&line.text, columns, &mut case_replies, policy)
                        .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                if kind == "output" && translated.is_some() {
                    replies = case_replies.clone();
                    if let Some(replay) = operations
                        .iter_mut()
                        .rev()
                        .find(|replay| replay.reply_after.is_some())
                    {
                        replay.reply_after = Some(case_replies.clone());
                    }
                }
                if let Some(expectation) = translated {
                    queue_expectation(
                        &mut pending,
                        expectation,
                        Some(kind),
                        &mut unconverted_kinds,
                    )
                    .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                } else {
                    unconverted_kinds.push(kind.to_string());
                    unsupported_property |= kind == "settermprop";
                    if let Some(reason) = unsupported_observation_reason(kind, &line.text) {
                        unsupported = Some(reason);
                    }
                }
                continue;
            }

            flush_expectations(
                &mut pending,
                &mut steps,
                &mut converted,
                &mut retained_converted_kinds,
            );
            let command = command_name(&line.text)
                .ok_or_else(|| format!("{}:{}: command has no name", file.path, line.number))?;
            let rest = line.text.strip_prefix(command).unwrap_or("").trim();
            match command {
                "RESET" => {
                    operations.clear();
                    replies.clear();
                    case_replies.clear();
                    steps.clear();
                    converted = 0;
                    unconverted_kinds.append(&mut retained_converted_kinds);
                    baseline_rows = rows;
                    baseline_columns = columns;
                    case_rows = rows;
                    case_columns = columns;
                    profile_stream.clear();
                    unsupported = profile_exclusion;
                    unsupported_property = false;
                    output_assertion = false;
                    last_write = None;
                    operation_snapshot = operations.clone();
                    reply_snapshot = replies.clone();
                    row_snapshot = rows;
                    column_snapshot = columns;
                    baseline_row_snapshot = baseline_rows;
                    baseline_column_snapshot = baseline_columns;
                }
                "PUSH" => {
                    let bytes = parse_perl_bytes(rest)
                        .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                    last_write = Some(bytes.clone());
                    let profile_settled = scan_protocol(&profile_stream).settled;
                    let profile_boundary = profile_stream.len();
                    profile_stream.extend_from_slice(&bytes);
                    if let Some(reason) = excluded_protocol(&profile_stream) {
                        profile_stream.truncate(profile_boundary);
                        if (!profile_settled || !standalone_prunable_query(&bytes))
                            && unsupported.is_none()
                        {
                            unsupported = Some(reason);
                        }
                        continue;
                    }
                    let operation = Operation::Write(bytes);
                    operations.push(ReplayOperation {
                        operation: operation.clone(),
                        reply_after: Some(case_replies.clone()),
                    });
                    steps.push(NativeStep::Operation(operation));
                    queue_expectation(
                        &mut pending,
                        format!(
                            "expect reply {}",
                            native_escape_bytes(case_replies.as_slice())
                        ),
                        None,
                        &mut unconverted_kinds,
                    )
                    .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                }
                "RESIZE" => {
                    let size = parse_resize(rest)
                        .map_err(|error| format!("{}:{}: {error}", file.path, line.number))?;
                    rows = size.0;
                    columns = size.1;
                    let operation = Operation::Resize(rows, columns);
                    operations.push(ReplayOperation {
                        operation: operation.clone(),
                        reply_after: None,
                    });
                    steps.push(NativeStep::Operation(operation));
                }
                _ if ignored_configuration(command) => {}
                _ => {
                    unsupported = Some(unsupported_command_reason(command));
                }
            }
        }
        flush_expectations(
            &mut pending,
            &mut steps,
            &mut converted,
            &mut retained_converted_kinds,
        );
        if unsupported_property && output_assertion {
            unsupported = Some("unsupported terminal property query is outside the first profile");
        }
        if unsupported.is_none() && !scan_protocol(&profile_stream).settled {
            unsupported = Some("incomplete terminal input at source section boundary");
        }
        if unsupported.is_none() {
            unsupported = excluded_protocol_steps(&steps);
        }

        if let Some(reason) = unsupported {
            record_case_exclusion(counts, source, reason);
            for kind in assertion_kinds {
                count_exclusion(&mut counts.excluded_assertions, kind);
            }
            operations = operation_snapshot;
            replies = reply_snapshot;
            rows = row_snapshot;
            columns = column_snapshot;
            baseline_rows = baseline_row_snapshot;
            baseline_columns = baseline_column_snapshot;
            if let Some(bytes) = excluded_case_replay(name, &case.title) {
                operations.push(ReplayOperation {
                    operation: Operation::Write(bytes.to_vec()),
                    reply_after: Some(replies.clone()),
                });
            }
            execute_state_lines(
                (&file.path, postlude, false),
                &mut operations,
                &mut replies,
                &mut baseline_rows,
                &mut baseline_columns,
                &mut rows,
                &mut columns,
            )?;
            continue;
        }
        let operations_in_case = steps
            .iter()
            .filter(|step| matches!(step, NativeStep::Operation(_)))
            .count();
        if converted == 0 || operations_in_case == 0 {
            record_case_exclusion(
                counts,
                source,
                "no supported externally observable assertion",
            );
            for kind in assertion_kinds {
                count_exclusion(&mut counts.excluded_assertions, kind);
            }
            execute_state_lines(
                (&file.path, postlude, true),
                &mut operations,
                &mut replies,
                &mut baseline_rows,
                &mut baseline_columns,
                &mut rows,
                &mut columns,
            )?;
            continue;
        }
        for kind in unconverted_kinds {
            count_exclusion(&mut counts.excluded_assertions, kind);
        }
        let id = unique_id(&file.path, &case.title, ids)?;
        counts.native_cases = counts.native_cases.saturating_add(1);
        counts.converted_assertions = counts.converted_assertions.saturating_add(converted);
        native.push(NativeCase {
            id,
            source,
            tags: tags_for(name),
            rows: case_rows,
            columns: case_columns,
            steps,
        });
        execute_state_lines(
            (&file.path, postlude, true),
            &mut operations,
            &mut replies,
            &mut baseline_rows,
            &mut baseline_columns,
            &mut rows,
            &mut columns,
        )?;
    }
    Ok(())
}

fn render_report(counts: &Counts, native: &str) -> String {
    let mut report = String::new();
    report.push_str("td-term libvterm migration report\n");
    let _ = writeln!(report, "release {RELEASE}");
    let _ = writeln!(report, "tag-commit {TAG_COMMIT}");
    let _ = writeln!(report, "archive-url {ARCHIVE_URL}");
    let _ = writeln!(report, "archive-sha256 {ARCHIVE_SHA256}");
    let _ = writeln!(
        report,
        "source-manifest-sha256 {}",
        sha256_hex(SOURCE_MANIFEST.as_bytes())
    );
    let _ = writeln!(report, "native-sha256 {}", sha256_hex(native.as_bytes()));
    let _ = writeln!(report, "source-files {}", counts.source_files);
    let _ = writeln!(report, "source-cases {}", counts.source_cases);
    let _ = writeln!(report, "source-assertions-raw {}", counts.raw_assertions);
    let _ = writeln!(
        report,
        "source-assertions-expanded {}",
        counts.expanded_assertions
    );
    let _ = writeln!(report, "native-cases {}", counts.native_cases);
    let _ = writeln!(
        report,
        "converted-assertions {}",
        counts.converted_assertions
    );
    let excluded_cases = counts
        .excluded_cases
        .values()
        .fold(0usize, |total, count| total.saturating_add(*count));
    let excluded_assertions = counts
        .excluded_assertions
        .values()
        .fold(0usize, |total, count| total.saturating_add(*count));
    let _ = writeln!(report, "excluded-cases {excluded_cases}");
    let _ = writeln!(report, "excluded-assertions {excluded_assertions}");
    report.push_str("\ncase-exclusion-counts\n");
    for (reason, count) in &counts.excluded_cases {
        let _ = writeln!(report, "{count} {reason}");
    }
    report.push_str("\nassertion-exclusion-counts\n");
    for (kind, count) in &counts.excluded_assertions {
        let _ = writeln!(report, "{count} {kind}");
    }
    report.push_str("\nexcluded-cases-detail\n");
    for (source, reason) in &counts.excluded_case_records {
        let _ = writeln!(report, "{source}\t{reason}");
    }
    report
}

pub(crate) fn generate_tree(root: &Path) -> Result<Generated, String> {
    let files = parse_sources(load_sources(root)?)?;
    let mut counts = Counts {
        source_files: files.len(),
        ..Counts::default()
    };
    for file in &files {
        counts.source_cases = counts.source_cases.saturating_add(file.cases.len());
        counts.raw_assertions = counts.raw_assertions.saturating_add(file.raw_assertions);
        counts.expanded_assertions = counts
            .expanded_assertions
            .saturating_add(file.expanded_assertions);
    }
    let mut ids = BTreeSet::new();
    let mut cases = Vec::new();
    for file in &files {
        convert_file(file, &mut ids, &mut cases, &mut counts)?;
    }
    validate_native_profile(&cases)?;
    cases.sort_by(|left, right| left.id.cmp(&right.id));
    counts.excluded_case_records.sort();
    let mut native = format!(
        "# Generated from {RELEASE} ({TAG_COMMIT}).\n\
         # Regenerate with tools/import-libvterm.rs; do not edit imported cases by hand.\n\n"
    );
    for case in &cases {
        render_case(case, &mut native);
    }
    if native.ends_with("\n\n") {
        native.pop();
    }
    let report = render_report(&counts, &native);
    Ok(Generated { native, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perl_string_subset_is_bounded_and_exact() {
        assert_eq!(parse_perl_bytes("\"A\\e\" . \"x\"x3").unwrap(), b"A\x1bxxx");
        assert_eq!(parse_perl_bytes("\"\\x{9d}\\x41\"").unwrap(), b"\x9dA");
        assert!(parse_perl_bytes("\"x\" y 2").is_err());
        assert!(parse_perl_bytes("\"x\"x4097").is_err());
    }

    #[test]
    fn source_parser_rejects_unknown_vocabulary() {
        let source = "INIT\n!case\nPUSH \"x\"\n  mystery 1\n";
        assert!(parse_source_file("t/a.test", source)
            .unwrap_err()
            .contains("unknown assertion"));
    }

    #[test]
    fn native_escaping_round_trips_the_importer_byte_vocabulary() {
        assert_eq!(
            native_escape_bytes(b"a\x1b\\\"\0\xff"),
            "b\"a\\e\\\\\\\"\\0\\xff\""
        );
    }

    #[test]
    fn output_recovery_moves_old_files_and_preserves_failed_backups() {
        let directory = std::env::temp_dir().join(format!(
            "td-term-import-output-recovery-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let output = directory.join("output");
        let backup = directory.join("backup");
        std::fs::write(&output, b"old").unwrap();
        assert!(move_output_to_backup(&output, &backup).unwrap());
        std::fs::write(&output, b"new").unwrap();
        restore_output(&output, &backup, true).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"old");
        assert!(!backup.exists());

        std::fs::remove_file(&output).unwrap();
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(&output, b"new").unwrap();
        assert!(restore_output(&output, &backup, true).is_err());
        assert!(backup.is_dir());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn profile_crossing_observations_exclude_their_case() {
        assert!(unsupported_observation_reason("putglyph", "putglyph 0xff10 2 0,0").is_some());
        assert!(unsupported_observation_reason(
            "screen_cell",
            "?screen_cell 0,0 = {0x65,0x301} width=1 attrs={}"
        )
        .is_some());
        assert!(unsupported_observation_reason(
            "screen_cell",
            "?screen_cell 0,0 = {0x41} width=1 attrs={F1}"
        )
        .is_none());
        assert!(unsupported_observation_reason("settermprop", "settermprop 7 1").is_none());
        assert!(case_profile_exclusion("14state_encoding.test", "Designate G2 + LS2").is_some());
        assert!(case_profile_exclusion("14state_encoding.test", "Designate G1 + LS1").is_none());
        assert!(case_profile_exclusion("26state_query.test", "DECRQSS on SGR").is_some());
        assert!(case_profile_exclusion("26state_query.test", "CPR").is_none());
        assert_eq!(
            excluded_protocol(b"\x1b[?69h"),
            Some("horizontal-margin mode")
        );
        assert_eq!(excluded_protocol(b"\x1b#8"), Some("DEC screen alignment"));
        assert_eq!(excluded_protocol(b"\x1b[?1K"), Some("selective line erase"));
        assert_eq!(
            excluded_protocol(b"\x1b[?2J"),
            Some("selective display erase")
        );
        assert_eq!(
            excluded_protocol(b"\x1b]2;terminal title\x07"),
            Some("terminal title")
        );
        assert_eq!(
            excluded_protocol(b"\x1b)A"),
            Some("unsupported G0/G1 character set")
        );
        assert!(excluded_protocol(b"\xc4\x8e\xc4\x9d").is_none());
        let interrupted = scan_protocol(b"\xc3\r\x8e");
        assert!(interrupted.excluded.is_none());
        assert!(interrupted.settled);
        let interrupted = scan_protocol(b"\xc3\x19\x8e");
        assert!(interrupted.excluded.is_none());
        assert!(interrupted.settled);
        assert!(!scan_protocol(b"\xc3\x7f").settled);
        assert!(!scan_protocol(b"\xc3\x19").settled);
        assert!(excluded_protocol(b"\x1b[38;5;11m").is_none());
        assert_eq!(
            excluded_protocol(b"\x1b[38:5:11m"),
            Some("colon-separated color")
        );
        assert!(excluded_protocol(b"\x1b[48;2;1;2;73m").is_none());
        assert_eq!(excluded_protocol(b"\x1b[38;5;11;73m"), Some("superscript"));
        assert!(excluded_protocol(b"\x1bP$\x18q").is_none());
        assert_eq!(
            excluded_protocol(b"\x1bP$\x7fq"),
            Some("status-string query")
        );
        assert_eq!(
            excluded_protocol(b"\x1bP\x1b[?1K"),
            Some("selective line erase")
        );
        assert!(standalone_prunable_query(b"\x1b[?25$p"));
        assert!(!standalone_prunable_query(b"A\x1b[?25$pB"));
        assert!(!standalone_prunable_query(b"\x1b[1;11m"));
        assert!(!standalone_prunable_query(b"\x1b[?6;69h"));
        let interrupted_query = b"\x1bP$\rqm\x1b\\";
        assert!(standalone_prunable_query(interrupted_query));
        assert!(matches!(
            output_policy(Some(interrupted_query)),
            OutputPolicy::Unsupported
        ));
        assert!(excluded_protocol(b"\x1b[?25h").is_none());
        assert!(matches!(
            output_policy(Some(b"\x1b[c")),
            OutputPolicy::PrimaryDeviceAttributes
        ));
        assert!(matches!(
            output_policy(Some(b"\x1b[?6$p")),
            OutputPolicy::Unsupported
        ));
        let mut replies = Vec::new();
        assert_eq!(
            convert_output(
                "output \"\\e[?1;2c\"",
                &mut replies,
                OutputPolicy::PrimaryDeviceAttributes
            )
            .unwrap(),
            "expect reply b\"\\e[?1;0c\""
        );
        assert_eq!(replies, b"\x1b[?1;0c");
    }

    #[test]
    fn native_profile_validation_tracks_protocols_across_writes() {
        let make_case = |writes: &[&[u8]]| NativeCase {
            id: "split-protocol".to_string(),
            source: "test".to_string(),
            tags: vec!["core"],
            rows: 25,
            columns: 80,
            steps: writes
                .iter()
                .map(|bytes| NativeStep::Operation(Operation::Write(bytes.to_vec())))
                .collect(),
        };
        assert!(validate_native_profile(&[make_case(&[b"\x1b[?", b"1K"])]).is_err());
        assert!(validate_native_profile(&[make_case(&[b"\x1b[", b"?2J"])]).is_err());
        assert!(validate_native_profile(&[make_case(&[b"\x1b", b"]2;title\x07"])]).is_err());
        assert!(validate_native_profile(&[make_case(&[b"\x1b[?69", b"\x08h"])]).is_err());
        assert!(validate_native_profile(&[make_case(&[b"\xc4", b"\x8e\xc4", b"\x9d"])]).is_ok());
    }

    #[test]
    fn conversion_preserves_utf8_split_across_source_writes() {
        let source = "INIT\nWANTSTATE p\nRESET\n!split utf8\n\
                      PUSH \"\\xc4\"\nPUSH \"\\x8e\"\n  putglyph 0x10e 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        let case = native.first().unwrap();
        let writes: Vec<&[u8]> = case
            .steps
            .iter()
            .filter_map(|step| match step {
                NativeStep::Operation(Operation::Write(bytes)) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![b"\xc4".as_slice(), b"\x8e".as_slice()]);
        assert_eq!(counts.native_cases, 1);
        assert_eq!(counts.converted_assertions, 1);
    }

    #[test]
    fn conversion_applies_trailing_reset_after_retaining_the_case() {
        let source = "INIT\nWANTSTATE p\nRESET\n!before reset\n\
                      PUSH \"A\"\n  putglyph 0x41 1 0,0\nRESET\n\
                      !after reset\nPUSH \"B\"\n  putglyph 0x42 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert_eq!(native.len(), 2);
        assert_eq!(counts.native_cases, 2);
        assert_eq!(counts.converted_assertions, 2);
        assert!(native.first().unwrap().steps.iter().any(
            |step| matches!(step, NativeStep::Operation(Operation::Write(bytes)) if bytes == b"A")
        ));
        assert!(native.get(1).unwrap().steps.iter().any(
            |step| matches!(step, NativeStep::Operation(Operation::Write(bytes)) if bytes == b"B")
        ));
    }

    #[test]
    fn conversion_drops_excluded_sections_trailing_restoration_write() {
        let source = "INIT\nWANTSTATE p\nRESET\n!Origin mode with DECSLRM\n\
                      PUSH \"\\e[?69h\"\n  ?cursor = 0,0\nPUSH \"\\e[?69l\"\n\
                      !DEC origin mode\nPUSH \"\\e[?6h\"\n  ?cursor = 0,0\n";
        let file = parse_source_file("t/15state_mode.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert_eq!(native.len(), 1);
        assert_eq!(native.first().unwrap().id, "libvterm/15state-mode/dec-origin-mode");
        assert_eq!(
            counts.excluded_cases.get(
                "insert, newline, query, and horizontal-margin modes are outside the first profile"
            ),
            Some(&1)
        );
        assert_eq!(counts.excluded_cases.get("horizontal-margin mode"), None);
    }

    #[test]
    fn conversion_excludes_protocol_split_across_source_writes() {
        let source = "INIT\nWANTSTATE p\nRESET\n!split protocol\n\
                      PUSH \"\\e[\"\nPUSH \"?69h\\e[?25\\$p\"\n  putglyph 0x41 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert!(native.is_empty());
        assert_eq!(
            counts.excluded_cases.get("horizontal-margin mode"),
            Some(&1)
        );
        assert_eq!(counts.excluded_assertions.get("putglyph"), Some(&1));
    }

    #[test]
    fn conversion_excludes_mixed_utf8_and_deferred_query() {
        let source = "INIT\nWANTSTATE p\nRESET\n!mixed split\n\
                      PUSH \"\\xc4\"\nPUSH \"\\x8e\\e[?25\\$p\"\n  putglyph 0x10e 1 0,0\n\
                      !mixed ground\nRESET\nPUSH \"A\\e[?25\\$pB\"\n  putglyph 0x41 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert!(native.is_empty());
        assert_eq!(counts.excluded_cases.get("mode query"), Some(&2));
        assert_eq!(counts.excluded_assertions.get("putglyph"), Some(&2));
    }

    #[test]
    fn conversion_does_not_prune_query_after_interrupted_utf8() {
        let source = "INIT\nWANTSTATE p\nRESET\n!mixed pending\n\
                      PUSH \"\\xc3\\x19\\x7f\"\nPUSH \"\\e[?25\\$p\"\n  putglyph 0x41 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert!(native.is_empty());
        assert_eq!(counts.excluded_cases.get("mode query"), Some(&1));
        assert_eq!(counts.excluded_assertions.get("putglyph"), Some(&1));
    }

    #[test]
    fn conversion_pruned_interrupted_query_does_not_attach_output() {
        let source = "INIT\nWANTSTATE p\nRESET\n!query\n\
                      PUSH \"A\"\n  putglyph 0x41 1 0,0\n\
                      PUSH \"\\eP\\$\\rqm\\e\\\\\"\n  output \"reply\"\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        let case = native.first().unwrap();
        let writes = case
            .steps
            .iter()
            .filter_map(|step| match step {
                NativeStep::Operation(Operation::Write(bytes)) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(writes, vec![b"A".as_slice()]);
        assert_eq!(counts.converted_assertions, 1);
        assert_eq!(counts.excluded_assertions.get("output"), Some(&1));
    }

    #[test]
    fn conversion_excludes_stateful_mixed_parameters() {
        let source = "INIT\nWANTSTATE p\nRESET\n!mixed sgr\n\
                      PUSH \"\\e[1;11m\"\n  putglyph 0x41 1 0,0\n\
                      !mixed private modes\nRESET\nPUSH \"\\e[?6;69h\"\n  putglyph 0x41 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        assert!(native.is_empty());
        assert_eq!(counts.excluded_cases.get("alternate font"), Some(&1));
        assert_eq!(
            counts.excluded_cases.get("horizontal-margin mode"),
            Some(&1)
        );
        assert_eq!(counts.excluded_assertions.get("putglyph"), Some(&2));
    }

    #[test]
    fn conversion_rolls_back_unsettled_cross_section_prefix() {
        let source = "INIT\nWANTSTATE p\nRESET\n!prefix\nPUSH \"\\e[\"\n\
                      !completion\nPUSH \"?69h\"\n  putglyph 0x3f 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        let case = native.first().unwrap();
        let writes: Vec<&[u8]> = case
            .steps
            .iter()
            .filter_map(|step| match step {
                NativeStep::Operation(Operation::Write(bytes)) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![b"?69h".as_slice()]);
        assert_eq!(
            counts
                .excluded_cases
                .get("incomplete terminal input at source section boundary"),
            Some(&1)
        );
    }

    #[test]
    fn conversion_rolls_back_utf8_prefix_interrupted_by_del() {
        let source = "INIT\nWANTSTATE p\nRESET\n!prefix\nPUSH \"\\xc3\\x19\\x7f\"\n\
                      !replacement\nPUSH \"\\xa9\"\n  putglyph 0xfffd 1 0,0\n";
        let file = parse_source_file("t/10state_putglyph.test", source).unwrap();
        let mut ids = BTreeSet::new();
        let mut native = Vec::new();
        let mut counts = Counts::default();
        convert_file(&file, &mut ids, &mut native, &mut counts).unwrap();
        let case = native.first().unwrap();
        let writes: Vec<&[u8]> = case
            .steps
            .iter()
            .filter_map(|step| match step {
                NativeStep::Operation(Operation::Write(bytes)) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![b"\xa9".as_slice()]);
        assert_eq!(
            counts
                .excluded_cases
                .get("incomplete terminal input at source section boundary"),
            Some(&1)
        );
    }

    #[test]
    fn source_reader_rejects_oversized_files_before_hashing() {
        let path = std::env::temp_dir().join(format!(
            "td-term-import-source-limit-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            vec![0u8; usize::try_from(MAX_SOURCE_FILE_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(source_file_size(&path)
            .unwrap_err()
            .contains("source limit"));
        assert!(read_source_file(&path)
            .unwrap_err()
            .contains("source limit"));
        std::fs::remove_file(path).unwrap();
    }
}

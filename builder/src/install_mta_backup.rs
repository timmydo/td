//! Host-only bounded logical backups using the operator's PostgreSQL tools.
use super::{absolute, directory, text, Result};
use crate::json::Json;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str = "Usage: ./install-td-mta --backup --service-file PATH --service NAME\n  --destination PATH [--max-archive-bytes N] [--max-total-bytes N] [--retain N]\n\nRuns PostgreSQL 17 pg_dump and validates its archive directory with pg_restore.\nDestination must already exist, be owned by this user and have mode 0700.\nThe libpq service/password files must be provisioned separately (DEPLOYMENT.md).\nDefault bounds: 1 GiB per archive, 8 GiB total, 30 completed backups.\nRetains the newest completed backup even when space admission refuses.\nNever restores a database or activates mail. Schedule this command explicitly.\n";
const GIB: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const CHUNK: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(600);

struct Options {
    service_file: PathBuf,
    service: String,
    destination: PathBuf,
    archive_bytes: u64,
    total_bytes: u64,
    retain: usize,
}

fn options(args: &[String]) -> Result<Options> {
    let mut values = std::collections::BTreeMap::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if ![
            "--service-file",
            "--service",
            "--destination",
            "--max-archive-bytes",
            "--max-total-bytes",
            "--retain",
        ]
        .contains(&flag.as_str())
        {
            return Err(format!("unknown backup option: {flag}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        if values.insert(flag.as_str(), value.as_str()).is_some() {
            return Err(format!("duplicate backup option: {flag}"));
        }
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing {key}"))
    };
    let number = |key, default| -> Result<u64> {
        values.get(key).map_or(Ok(default), |v| {
            v.parse().map_err(|_| format!("invalid {key}"))
        })
    };
    let service = required("--service")?;
    if service.is_empty()
        || service.len() > 64
        || !service
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err("service must be a short ASCII libpq service name".into());
    }
    let out = Options {
        service_file: absolute(required("--service-file")?)?,
        service: service.into(),
        destination: absolute(required("--destination")?)?,
        archive_bytes: number("--max-archive-bytes", GIB)?,
        total_bytes: number("--max-total-bytes", 8 * GIB)?,
        retain: usize::try_from(number("--retain", 30)?).map_err(|_| "invalid retention")?,
    };
    if out.archive_bytes == 0
        || out.archive_bytes > 1024 * GIB
        || out.total_bytes < out.archive_bytes.saturating_mul(2) + 65536
        || !(1..=1000).contains(&out.retain)
    {
        return Err("bounds require a nonzero archive ceiling up to 1024 GiB, space for two archives plus 64 KiB, and 1..1000 retained backups".into());
    }
    Ok(out)
}

fn command(program: &str, opts: &Options) -> Command {
    let mut cmd = super::install_mta_setup::command(Path::new(program));
    cmd.env("PGSERVICEFILE", &opts.service_file)
        .env("PGSERVICE", &opts.service)
        .env("PGCONNECT_TIMEOUT", "10")
        .env("PGAPPNAME", "td-mta-backup");
    cmd
}

// Only one lock owner touches these private files. The service UID is trusted.
fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    Ok(file)
}

struct Temporary {
    path: PathBuf,
    file: File,
    retained: bool,
}
impl Temporary {
    fn create(path: PathBuf) -> Result<Self> {
        let file = private_file(&path)?;
        Ok(Self {
            path,
            file,
            retained: false,
        })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.retained {
            let _ = fs::remove_file(&self.path);
        }
    }
}
type PipeMessage = std::result::Result<(bool, usize, [u8; CHUNK]), String>;

fn bounded_output(cmd: Command, output: &mut File, maximum: u64, diagnostic: &Path) -> Result<u64> {
    let (bytes, status) = bounded_tool_output(cmd, output, maximum, diagnostic, TIMEOUT)?;
    if status.success() {
        Ok(bytes)
    } else {
        Err(format!(
            "PostgreSQL tool failed ({status}); private diagnostic: {}",
            diagnostic.display()
        ))
    }
}

pub(super) fn bounded_tool_output(
    mut cmd: Command,
    output: &mut File,
    maximum: u64,
    diagnostic: &Path,
    timeout: Duration,
) -> Result<(u64, std::process::ExitStatus)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start mail host tool: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("mail host tool has no output pipe")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("mail host tool has no diagnostic pipe")?;
    let until = Instant::now() + timeout;
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let read_pipe = |mut pipe: Box<dyn Read + Send>,
                         stderr,
                         sender: std::sync::mpsc::SyncSender<PipeMessage>| {
            loop {
                let mut bytes = [0; CHUNK];
                match pipe.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(Ok((stderr, n, bytes))).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = sender.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        };
        let errors = sender.clone();
        let reader = match std::thread::Builder::new()
            .name("backup-output".into())
            .spawn_scoped(scope, move || read_pipe(Box::new(stdout), false, sender))
        {
            Ok(reader) => reader,
            Err(e) => {
                let _ = crate::sys::kill_child_recorded(
                    &mut child,
                    "mail tool output reader could not start",
                );
                let _ = child.wait();
                return Err(format!("start mail tool output reader: {e}"));
            }
        };
        let error_reader = match std::thread::Builder::new()
            .name("backup-diagnostic".into())
            .spawn_scoped(scope, move || read_pipe(Box::new(stderr), true, errors))
        {
            Ok(reader) => reader,
            Err(e) => {
                drop(receiver);
                let _ = crate::sys::kill_child_recorded(
                    &mut child,
                    "mail tool diagnostic reader could not start",
                );
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("start mail tool diagnostic reader: {e}"));
            }
        };
        let mut written = 0u64;
        let mut error_bytes = Vec::with_capacity(16384);
        let transfer = (|| {
            loop {
                let left = until
                    .checked_duration_since(Instant::now())
                    .ok_or("mail tool deadline exceeded")?;
                match receiver.recv_timeout(left) {
                    Ok(Ok((stderr, n, bytes))) => {
                        let bytes = bytes.get(..n).ok_or("invalid output length")?;
                        if stderr {
                            error_bytes.extend_from_slice(
                                bytes
                                    .get(..bytes.len().min(16384 - error_bytes.len()))
                                    .ok_or("invalid diagnostic length")?,
                            );
                            continue;
                        }
                        written = written
                            .checked_add(n as u64)
                            .ok_or("mail tool size overflow")?;
                        if written > maximum {
                            return Err("mail tool output exceeds configured ceiling".into());
                        }
                        output
                            .write_all(bytes)
                            .map_err(|e| format!("write tool output: {e}"))?;
                    }
                    Ok(Err(e)) => return Err(format!("read mail host tool output: {e}")),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(_) => return Err("mail tool deadline exceeded".into()),
                }
            }
            loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    return Ok((written, status));
                }
                if Instant::now() >= until {
                    return Err("mail tool deadline exceeded".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })();
        drop(receiver);
        if transfer.is_err() {
            let _ = crate::sys::kill_child_recorded(
                &mut child,
                "mail host tool failed, exceeded a limit, or could not write output",
            );
        }
        let _ = child.wait();
        reader
            .join()
            .map_err(|_| "mail tool output reader failed".to_string())?;
        error_reader
            .join()
            .map_err(|_| "mail tool diagnostic reader failed".to_string())?;
        if !transfer.as_ref().is_ok_and(|(_, status)| status.success()) {
            let mut report = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(diagnostic)
                .map_err(|e| e.to_string())?;
            report.write_all(&error_bytes).map_err(|e| e.to_string())?;
        }
        transfer
    })
}

struct Archive {
    path: PathBuf,
    stamp: u64,
    bytes: u64,
}
fn inventory(destination: &Path) -> Result<(Vec<Archive>, u64, usize)> {
    let mut archives = Vec::new();
    let mut total = 0u64;
    let mut entries = 0;
    for (i, entry) in fs::read_dir(destination)
        .map_err(|e| e.to_string())?
        .enumerate()
    {
        if i >= MAX_ENTRIES {
            return Err("backup directory entry ceiling exceeded".into());
        }
        entries += 1;
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != crate::sys::getuid()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("backup directory must contain only private, owned regular files".into());
        }
        total = total
            .checked_add(metadata.len())
            .ok_or("backup size overflow")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("backup filename is not UTF-8")?;
        if let Some(stem) = name
            .strip_prefix("backup-")
            .and_then(|v| v.strip_suffix(".dump"))
        {
            let mut parts = stem.split('-');
            if let (Some(stamp), Some(pid), None) = (parts.next(), parts.next(), parts.next()) {
                if let (Ok(stamp), Ok(_)) = (stamp.parse::<u64>(), pid.parse::<u32>()) {
                    archives.push(Archive {
                        path: entry.path(),
                        stamp,
                        bytes: metadata.len(),
                    });
                }
            }
        }
    }
    archives.sort_by(|a, b| a.stamp.cmp(&b.stamp).then(a.path.cmp(&b.path)));
    Ok((archives, total, entries))
}

fn cleanup_interrupted(destination: &Path) -> Result<()> {
    // Validate every entry before removing any owned scratch. The exclusive
    // destination lock excludes another live producer; children die with it.
    inventory(destination)?;
    for entry in fs::read_dir(destination).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|v| v.to_str()) else {
            continue;
        };
        if !["partial", "validation", "version", "json"].contains(&extension) {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|v| v.to_str())
            .and_then(|v| v.strip_prefix("backup-"))
        else {
            continue;
        };
        let mut parts = stem.split('-');
        let owned = matches!((parts.next(), parts.next(), parts.next()),
            (Some(stamp), Some(pid), None) if stamp.parse::<u64>().is_ok() && pid.parse::<u32>().is_ok());
        if owned && (extension != "json" || !path.with_extension("dump").exists()) {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn prune(destination: &Path, reserve: u64, maximum: u64, retain: usize) -> Result<()> {
    let (archives, mut total, mut entries) = inventory(destination)?;
    // A pending attempt needs room for scratch, publication and diagnostics.
    let entry_reserve = if reserve == 0 { 0 } else { 4 };
    let count = archives.len();
    for (i, archive) in archives.into_iter().enumerate() {
        if count - i <= retain
            && total.checked_add(reserve).is_some_and(|n| n <= maximum)
            && entries + entry_reserve <= MAX_ENTRIES
        {
            break;
        }
        // A failed attempt may never remove the last completed archive.
        if count - i <= 1 {
            break;
        }
        fs::remove_file(&archive.path).map_err(|e| e.to_string())?;
        total = total.saturating_sub(archive.bytes);
        entries -= 1;
        let receipt = archive.path.with_extension("json");
        match fs::metadata(&receipt) {
            Ok(metadata) => {
                fs::remove_file(&receipt).map_err(|e| e.to_string())?;
                total = total.saturating_sub(metadata.len());
                entries -= 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    if total.checked_add(reserve).is_none_or(|n| n > maximum)
        || entries + entry_reserve > MAX_ENTRIES
    {
        return Err("backup capacity refused; newest archive retained".into());
    }
    Ok(())
}

fn tool_version(program: &str, opts: &Options, path: PathBuf) -> Result<String> {
    let mut output = Temporary::create(path)?;
    let mut cmd = command(program, opts);
    cmd.arg("--version");
    bounded_output(
        cmd,
        &mut output.file,
        512,
        &opts.destination.join("last-error"),
    )?;
    let value = fs::read_to_string(&output.path).map_err(|e| e.to_string())?;
    if !value.starts_with(&format!("{program} (PostgreSQL) 17.")) {
        return Err(format!("{program} must be PostgreSQL 17"));
    }
    Ok(value.trim().into())
}

fn destination_lock(destination: &Path) -> Result<File> {
    let lock_path = destination.join("lock");
    let (_, _, entries) = inventory(destination)?;
    match fs::symlink_metadata(&lock_path) {
        Ok(meta)
            if !meta.is_file()
                || meta.uid() != crate::sys::getuid()
                || meta.mode() & 0o077 != 0 =>
        {
            return Err("invalid backup lock file".into())
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if entries >= MAX_ENTRIES {
                return Err("no directory entry available for backup lock".into());
            }
        }
        Err(e) => return Err(e.to_string()),
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    if !crate::sys::flock_try_exclusive(lock.as_raw_fd()).map_err(|e| e.to_string())? {
        return Err("another backup owns this destination".into());
    }
    Ok(lock)
}

fn backup(opts: &Options) -> Result<()> {
    super::install_mta_setup::protected_input(&opts.service_file, 65536)?;
    super::install_mta_setup::trusted_ancestors(&opts.destination)?;
    directory(&opts.destination, 0o700)?;
    if fs::metadata(&opts.destination)
        .map_err(|e| e.to_string())?
        .uid()
        != crate::sys::getuid()
    {
        return Err("backup destination must be owned by this user".into());
    }
    let _lock = destination_lock(&opts.destination)?;
    cleanup_interrupted(&opts.destination)?;
    // Includes the archive receipt and bounded tool-validation output.
    let reserve = opts.archive_bytes + 65536;
    prune(&opts.destination, reserve, opts.total_bytes, opts.retain)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let (archives, _, _) = inventory(&opts.destination)?;
    let sequence = match archives.last() {
        Some(last) => stamp.max(
            last.stamp
                .checked_add(1)
                .ok_or("backup sequence exhausted")?,
        ),
        None => stamp,
    };
    let stem = format!("backup-{sequence}-{}", std::process::id());
    let staging = opts.destination.join(format!("{stem}.partial"));
    let validation = opts.destination.join(format!("{stem}.validation"));
    let final_path = opts.destination.join(format!("{stem}.dump"));
    let receipt_path = opts.destination.join(format!("{stem}.json"));
    let dump_version = tool_version(
        "pg_dump",
        opts,
        opts.destination.join(format!("{stem}.version")),
    )?;
    let restore_version = tool_version(
        "pg_restore",
        opts,
        opts.destination.join(format!("{stem}.version")),
    )?;
    let mut output = Temporary::create(staging.clone())?;
    let result = (|| {
        let mut dump = command("pg_dump", opts);
        dump.args(["--no-password", "--format=custom"]);
        let bytes = bounded_output(
            dump,
            &mut output.file,
            opts.archive_bytes,
            &opts.destination.join("last-error"),
        )?;
        output.file.sync_all().map_err(|e| e.to_string())?;
        let mut checked = Temporary::create(validation.clone())?;
        let mut restore = command("pg_restore", opts);
        restore.arg("--list").arg(&staging);
        bounded_output(
            restore,
            &mut checked.file,
            32768,
            &opts.destination.join("last-error"),
        )?;
        drop(checked);
        let hash = td_engine::sha256::sha256_file(&staging).map_err(|e| e.to_string())?;
        let report = Json::Obj(vec![
            ("schema".into(), Json::Num("1".into())),
            ("created_unix_seconds".into(), Json::Num(stamp.to_string())),
            ("bytes".into(), Json::Num(bytes.to_string())),
            ("sha256".into(), Json::Str(hash)),
            ("archive".into(), Json::Str(text(&final_path)?.into())),
            ("restored".into(), Json::Bool(false)),
            ("pg_dump".into(), Json::Str(dump_version)),
            ("pg_restore".into(), Json::Str(restore_version)),
        ])
        .to_canonical();
        let mut receipt = Temporary::create(receipt_path.clone())?;
        receipt
            .file
            .write_all(report.as_bytes())
            .and_then(|_| receipt.file.sync_all())
            .map_err(|e| e.to_string())?;
        // No existing final archive is replaced, even after PID/clock reuse.
        fs::hard_link(&staging, &final_path).map_err(|e| e.to_string())?;
        receipt.retained = true;
        fs::remove_file(&staging).map_err(|e| e.to_string())?;
        File::open(&opts.destination)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        prune(&opts.destination, 0, opts.total_bytes, opts.retain)?;
        match fs::remove_file(opts.destination.join("last-error")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
        println!("{report}");
        Ok(())
    })();
    result
}

pub(super) fn run(args: &[String]) -> ExitCode {
    if args == ["--help"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match options(args).and_then(|opts| backup(&opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("install-td-mta backup: {e}");
            ExitCode::FAILURE
        }
    }
}

// Readiness observes the receipt and file length; it does not verify contents.
fn receipt_created(input: &str, archive: &Archive) -> Result<u64> {
    let receipt = crate::json::parse(input).map_err(|_| "invalid backup receipt")?;
    let number = |key| match receipt.get(key) {
        Some(Json::Num(value)) => value.parse::<u64>().map_err(|_| "invalid receipt number"),
        _ => Err("missing receipt number"),
    };
    if number("schema")? != 1
        || number("bytes")? != archive.bytes
        || archive.bytes == 0
        || receipt.get("archive").and_then(Json::as_str) != archive.path.to_str()
    {
        return Err("backup receipt does not match archive".into());
    }
    Ok(number("created_unix_seconds")?)
}

fn freshness(created: u64, now: u64, maximum_age: u64, last_error: bool) -> &'static str {
    if created > now {
        "future"
    } else if now - created > maximum_age {
        "stale"
    } else if last_error {
        "last-attempt-failed"
    } else {
        "ready"
    }
}

fn status_options(args: &[String]) -> Result<(PathBuf, u64)> {
    let mut destination = None;
    let mut age = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("missing backup status option value")?;
        match flag.as_str() {
            "--destination" if destination.is_none() => destination = Some(absolute(value)?),
            "--maximum-age-seconds" if age.is_none() => {
                let seconds = value.parse::<u64>().map_err(|_| "invalid backup age")?;
                if !(1..=31_536_000).contains(&seconds) {
                    return Err("backup age must be 1..31536000 seconds".into());
                }
                age = Some(seconds);
            }
            _ => return Err("unknown or duplicate backup status option".into()),
        }
    }
    Ok((
        destination.ok_or("missing --destination")?,
        age.unwrap_or(172800),
    ))
}

fn backup_status(destination: &Path, maximum_age: u64) -> Result<bool> {
    super::install_mta_setup::trusted_ancestors(destination)?;
    let metadata = fs::symlink_metadata(destination).map_err(|e| e.to_string())?;
    if metadata.uid() != crate::sys::getuid() || metadata.mode() & 0o777 != 0o700 {
        return Err("backup destination must be owned by this user with mode 0700".into());
    }
    let (archives, total, _) = inventory(destination)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let mut created = Json::Null;
    let mut age = Json::Null;
    let health = match archives.last() {
        None => "missing",
        Some(archive) => {
            let bytes = super::install_mta_setup::protected_input(
                &archive.path.with_extension("json"),
                65536,
            )?;
            let input = std::str::from_utf8(&bytes).map_err(|_| "invalid receipt encoding")?;
            let stamp = receipt_created(input, archive)?;
            created = Json::Num(stamp.to_string());
            if let Some(seconds) = now.checked_sub(stamp) {
                age = Json::Num(seconds.to_string());
            }
            let last_error = match fs::symlink_metadata(destination.join("last-error")) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.to_string()),
            };
            freshness(stamp, now, maximum_age, last_error)
        }
    };
    let report = Json::Obj(vec![
        ("schema".into(), Json::Num("1".into())),
        ("status".into(), Json::Str(health.into())),
        ("created_unix_seconds".into(), created),
        ("age_seconds".into(), age),
        ("archives".into(), Json::Num(archives.len().to_string())),
        ("directory_bytes".into(), Json::Num(total.to_string())),
        ("contents_verified".into(), Json::Bool(false)),
    ]);
    println!("{}", report.to_canonical());
    Ok(health == "ready")
}

pub(super) fn status(args: &[String]) -> ExitCode {
    if args == ["--help"] {
        println!("Usage: ./install-td-mta --backup-status --destination PATH [--maximum-age-seconds N]\n\nRead-only receipt/age check; defaults to 172800 seconds (two days).\nExits 1 unless a recent completed archive and matching receipt exist,\nwith no retained tool failure diagnostic. Does not hash or restore archives.");
        return ExitCode::SUCCESS;
    }
    match status_options(args).and_then(|(destination, age)| backup_status(&destination, age)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("install-td-mta backup status: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_require_headroom_and_bounded_retention() {
        let base = [
            "--service-file",
            "/private/service",
            "--service",
            "mail",
            "--destination",
            "/private/backups",
        ]
        .map(String::from);
        assert_eq!(options(&base).unwrap().retain, 30);
        for extra in [
            ["--retain", "0"],
            ["--retain", "1001"],
            ["--max-archive-bytes", "0"],
            ["--max-total-bytes", "1024"],
        ] {
            let args: Vec<_> = base
                .iter()
                .cloned()
                .chain(extra.map(String::from))
                .collect();
            assert!(options(&args).is_err());
        }
    }

    fn archive(root: &Path, stamp: u64, bytes: usize) -> PathBuf {
        let file = root.join(format!("backup-{stamp}-1.dump"));
        private_file(&file)
            .unwrap()
            .write_all(&vec![42; bytes])
            .unwrap();
        private_file(&file.with_extension("json"))
            .unwrap()
            .write_all(b"{}")
            .unwrap();
        file
    }

    #[test]
    fn retention_reserves_space_without_deleting_the_newest_backup() {
        let root = crate::test_temp::TempDir::new("backup-retention").unwrap();
        let old = archive(root.path(), 1, 100);
        let newer = archive(root.path(), 2, 100);
        let newest = archive(root.path(), 3, 100);
        prune(root.path(), 102, 310, 30).unwrap();
        assert!(!old.exists());
        assert!(newer.exists());
        assert!(newest.exists());
        assert!(prune(root.path(), 1000, 310, 30).is_err());
        assert!(!newer.exists());
        assert!(newest.exists());
        assert_eq!(fs::read(newest).unwrap(), vec![42; 100]);
    }

    #[test]
    fn entry_admission_refuses_repeated_attempts_without_poisoning_cleanup() {
        let root = crate::test_temp::TempDir::new("backup-entry-admission").unwrap();
        let newest = archive(root.path(), 1, 100);
        for i in 0..MAX_ENTRIES - 3 {
            private_file(&root.path().join(format!("note-{i}"))).unwrap();
        }
        for _ in 0..3 {
            assert!(prune(root.path(), 1024, 8192, 30).is_err());
            cleanup_interrupted(root.path()).unwrap();
            assert!(newest.exists());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), MAX_ENTRIES - 1);
        }
    }

    #[test]
    fn full_directory_refuses_lock_creation_without_growing() {
        let root = crate::test_temp::TempDir::new("backup-lock-admission").unwrap();
        for i in 0..MAX_ENTRIES {
            private_file(&root.path().join(format!("note-{i}"))).unwrap();
        }
        for _ in 0..3 {
            assert!(destination_lock(root.path()).is_err());
            assert!(!root.path().join("lock").exists());
            inventory(root.path()).unwrap();
        }
    }

    #[test]
    fn unknown_files_consume_capacity_and_links_refuse() {
        let root = crate::test_temp::TempDir::new("backup-inventory").unwrap();
        private_file(&root.path().join("operator-note"))
            .unwrap()
            .write_all(&[0; 200])
            .unwrap();
        let newest = archive(root.path(), 1, 100);
        assert!(prune(root.path(), 100, 350, 30).is_err());
        assert!(newest.exists());
        std::os::unix::fs::symlink(&newest, root.path().join("link")).unwrap();
        assert!(inventory(root.path()).is_err());
    }

    #[test]
    fn interrupted_cleanup_keeps_completed_receipts_and_unrelated_files() {
        let root = crate::test_temp::TempDir::new("backup-interrupted").unwrap();
        let completed = archive(root.path(), 1, 100);
        let partial = root.path().join("backup-2-1.partial");
        let abandoned_receipt = root.path().join("backup-2-1.json");
        let note = root.path().join("operator.json");
        for path in [&partial, &abandoned_receipt, &note] {
            private_file(path).unwrap();
        }
        cleanup_interrupted(root.path()).unwrap();
        assert!(completed.exists());
        assert!(completed.with_extension("json").exists());
        assert!(note.exists());
        assert!(!partial.exists());
        assert!(!abandoned_receipt.exists());
    }

    #[test]
    fn temporary_files_do_not_remove_preexisting_files() {
        let root = crate::test_temp::TempDir::new("backup-temporary").unwrap();
        let path = root.path().join("partial");
        let file = Temporary::create(path.clone()).unwrap();
        assert!(Temporary::create(path.clone()).is_err());
        assert!(path.exists());
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn pipe_fixture() {
        if let Ok(mode) = std::env::var("TD_MTA_BACKUP_FIXTURE") {
            if mode == "sleep" {
                std::thread::sleep(Duration::from_secs(5));
                return;
            }
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            for _ in 0..16 {
                if out.write_all(&[b'x'; 16384]).is_err() {
                    return;
                }
                if err.write_all(&[b'e'; 16384]).is_err() {
                    return;
                }
            }
            if mode == "fail" {
                std::process::exit(7);
            }
        }
    }

    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "install_mta::install_mta_backup::tests::pipe_fixture",
                "--nocapture",
            ])
            .env("TD_MTA_BACKUP_FIXTURE", mode)
            .stdin(Stdio::null());
        command
    }

    #[test]
    fn backup_stream_bounds_output_and_private_failure_diagnostics() {
        let root = crate::test_temp::TempDir::new("backup-stream").unwrap();
        let diagnostic = root.path().join("error");
        let mut output = private_file(&root.path().join("archive")).unwrap();
        assert!(bounded_output(fixture_command("ok"), &mut output, 100, &diagnostic).is_err());
        assert!(output.metadata().unwrap().len() <= 100);
        assert!(bounded_output(
            fixture_command("fail"),
            &mut output,
            1024 * 1024,
            &diagnostic
        )
        .is_err());
        assert_eq!(fs::metadata(&diagnostic).unwrap().len(), 16384);
        assert_eq!(
            fs::metadata(&diagnostic).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            bounded_output(fixture_command("ok"), &mut output, 1024 * 1024, &diagnostic).unwrap()
                > 262144
        );
    }
    #[test]
    fn bounded_tools_preserve_exit_status_and_enforce_short_deadline() {
        let root = crate::test_temp::TempDir::new("mail-tool-status").unwrap();
        let diagnostic = root.path().join("error");
        let mut output = private_file(&root.path().join("output")).unwrap();
        let (_, status) = bounded_tool_output(
            fixture_command("fail"),
            &mut output,
            1024 * 1024,
            &diagnostic,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(status.code(), Some(7));
        let began = Instant::now();
        assert!(bounded_tool_output(
            fixture_command("sleep"),
            &mut output,
            1024 * 1024,
            &diagnostic,
            Duration::from_millis(100)
        )
        .is_err());
        assert!(began.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn receipt_freshness_uses_recorded_time_and_requires_matching_archive() {
        let archive = Archive {
            path: PathBuf::from("/private/backup-9999-1.dump"),
            stamp: 9999,
            bytes: 12,
        };
        let receipt = r#"{"schema":1,"created_unix_seconds":100,"bytes":12,"archive":"/private/backup-9999-1.dump"}"#;
        assert_eq!(receipt_created(receipt, &archive).unwrap(), 100);
        assert_eq!(freshness(100, 200, 100, false), "ready");
        assert_eq!(freshness(100, 201, 100, false), "stale");
        assert_eq!(freshness(100, 99, 100, false), "future");
        assert_eq!(freshness(100, 200, 100, true), "last-attempt-failed");
        for bad in [
            receipt.replace("\"bytes\":12", "\"bytes\":13"),
            receipt.replace("9999-1", "9998-1"),
            receipt.replace("\"schema\":1", "\"schema\":2"),
            receipt.replace(
                "\"created_unix_seconds\":100",
                "\"created_unix_seconds\":-1",
            ),
        ] {
            assert!(receipt_created(&bad, &archive).is_err());
        }
    }

    #[test]
    fn backup_status_arguments_bound_age_and_reject_ambiguity() {
        let base = ["--destination", "/private/backups"].map(String::from);
        assert_eq!(status_options(&base).unwrap().1, 172800);
        for extra in [
            vec!["--maximum-age-seconds", "0"],
            vec!["--maximum-age-seconds", "31536001"],
            vec!["--destination", "/other"],
            vec!["--maximum-age-seconds", "1", "--maximum-age-seconds", "2"],
            vec!["--retain", "1"],
        ] {
            let args: Vec<_> = base
                .iter()
                .cloned()
                .chain(extra.into_iter().map(String::from))
                .collect();
            assert!(status_options(&args).is_err());
        }
    }
}
